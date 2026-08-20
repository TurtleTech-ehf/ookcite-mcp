use std::fmt;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};

#[derive(Clone, PartialEq, Eq)]
pub enum CredentialReference {
    Platform { service: String, account: String },
    Command { retrieve_command: String },
    File { path: PathBuf },
}

impl CredentialReference {
    pub fn config_env(&self) -> Vec<(String, String)> {
        match self {
            Self::Platform { service, account } => vec![
                ("OOKCITE_CREDENTIAL_STORE".into(), "platform".into()),
                ("OOKCITE_CREDENTIAL_SERVICE".into(), service.clone()),
                ("OOKCITE_CREDENTIAL_ACCOUNT".into(), account.clone()),
            ],
            Self::Command { retrieve_command } => vec![(
                "OOKCITE_API_KEY_COMMAND".into(),
                retrieve_command.clone(),
            )],
            Self::File { path } => vec![(
                "OOKCITE_API_KEY_FILE".into(),
                path.to_string_lossy().into_owned(),
            )],
        }
    }
}

impl fmt::Debug for CredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform { service, account } => formatter
                .debug_struct("Platform")
                .field("service", service)
                .field("account", account)
                .finish(),
            Self::Command { .. } => formatter
                .debug_struct("Command")
                .field("retrieve_command", &"[configured]")
                .finish(),
            Self::File { path } => formatter.debug_struct("File").field("path", path).finish(),
        }
    }
}

pub trait CredentialSink {
    fn store(&self, secret: &SecretString) -> anyhow::Result<CredentialReference>;

    fn cleanup(&self, _reference: &CredentialReference) -> anyhow::Result<()> {
        Ok(())
    }
}

pub trait KeyringBackend {
    fn get_password(&self, service: &str, account: &str) -> anyhow::Result<Option<SecretString>>;
    fn set_password(
        &self,
        service: &str,
        account: &str,
        secret: &SecretString,
    ) -> anyhow::Result<()>;
    fn delete_password(&self, service: &str, account: &str) -> anyhow::Result<()>;
}

pub struct SystemKeyring;

impl KeyringBackend for SystemKeyring {
    fn get_password(&self, service: &str, account: &str) -> anyhow::Result<Option<SecretString>> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|_| anyhow::anyhow!("platform credential store is unavailable"))?;
        match entry.get_password() {
            Ok(password) => Ok(Some(SecretString::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => anyhow::bail!("platform credential could not be read"),
        }
    }

    fn set_password(
        &self,
        service: &str,
        account: &str,
        secret: &SecretString,
    ) -> anyhow::Result<()> {
        keyring::Entry::new(service, account)
            .and_then(|entry| entry.set_password(secret.expose_secret()))
            .map_err(|_| anyhow::anyhow!("platform credential could not be stored"))
    }

    fn delete_password(&self, service: &str, account: &str) -> anyhow::Result<()> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|_| anyhow::anyhow!("platform credential store is unavailable"))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => anyhow::bail!("platform credential could not be removed"),
        }
    }
}

enum PlatformRollback {
    Delete,
    Restore(SecretString),
}

pub struct PlatformCredentialSink<'a, B: KeyringBackend> {
    backend: &'a B,
    service: String,
    account: String,
    replace: bool,
    rollback: Mutex<Option<PlatformRollback>>,
}

impl<'a, B: KeyringBackend> PlatformCredentialSink<'a, B> {
    pub fn new(
        backend: &'a B,
        service: impl Into<String>,
        account: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            service: service.into(),
            account: account.into(),
            replace: false,
            rollback: Mutex::new(None),
        }
    }

    pub fn allow_replace(mut self, replace: bool) -> Self {
        self.replace = replace;
        self
    }
}

impl<B: KeyringBackend> CredentialSink for PlatformCredentialSink<'_, B> {
    fn store(&self, secret: &SecretString) -> anyhow::Result<CredentialReference> {
        let mut rollback = self
            .rollback
            .lock()
            .map_err(|_| anyhow::anyhow!("platform credential transaction is unavailable"))?;
        if rollback.is_some() {
            anyhow::bail!("platform credential transaction is already active")
        }
        let existing = self.backend.get_password(&self.service, &self.account)?;
        if existing.is_some() && !self.replace {
            anyhow::bail!("platform credential already exists; use explicit replacement")
        }
        self.backend
            .set_password(&self.service, &self.account, secret)?;
        *rollback = Some(match existing {
            Some(value) => PlatformRollback::Restore(value),
            None => PlatformRollback::Delete,
        });
        Ok(CredentialReference::Platform {
            service: self.service.clone(),
            account: self.account.clone(),
        })
    }

    fn cleanup(&self, reference: &CredentialReference) -> anyhow::Result<()> {
        if reference
            != &CredentialReference::Platform {
                service: self.service.clone(),
                account: self.account.clone(),
            }
        {
            anyhow::bail!("credential reference does not belong to this sink")
        }
        let mut rollback = self
            .rollback
            .lock()
            .map_err(|_| anyhow::anyhow!("platform credential transaction is unavailable"))?;
        match rollback.as_ref() {
            Some(PlatformRollback::Delete) => self
                .backend
                .delete_password(&self.service, &self.account)?,
            Some(PlatformRollback::Restore(value)) => {
                self.backend
                    .set_password(&self.service, &self.account, value)?
            }
            None => return Ok(()),
        }
        *rollback = None;
        Ok(())
    }
}

pub struct StoreCommandSink {
    pub command: Vec<String>,
    pub retrieve_command: String,
}

impl CredentialSink for StoreCommandSink {
    fn store(&self, secret: &SecretString) -> anyhow::Result<CredentialReference> {
        let (program, arguments) = self
            .command
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("store command is empty"))?;
        let mut child = std::process::Command::new(program)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("store command has no standard input"))?;
        stdin.write_all(secret.expose_secret().as_bytes())?;
        stdin.write_all(b"\n")?;
        drop(stdin);
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("store command failed")
        }
        Ok(CredentialReference::Command {
            retrieve_command: self.retrieve_command.clone(),
        })
    }
}

pub struct ProtectedFileSink {
    path: PathBuf,
    created: Mutex<Option<CreatedFile>>,
}

#[derive(Clone, Copy)]
struct CreatedFile {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl ProtectedFileSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            created: Mutex::new(None),
        }
    }
}

impl CredentialSink for ProtectedFileSink {
    fn store(&self, secret: &SecretString) -> anyhow::Result<CredentialReference> {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&self.path)?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;

        writeln!(file, "{}", secret.expose_secret())?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::MetadataExt as _;
            CreatedFile {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        };
        #[cfg(not(unix))]
        let created = {
            let _ = metadata;
            CreatedFile {}
        };
        *self
            .created
            .lock()
            .map_err(|_| anyhow::anyhow!("credential file transaction is unavailable"))? =
            Some(created);
        Ok(CredentialReference::File {
            path: self.path.clone(),
        })
    }

    fn cleanup(&self, reference: &CredentialReference) -> anyhow::Result<()> {
        if reference
            != &CredentialReference::File {
                path: self.path.clone(),
            }
        {
            anyhow::bail!("credential reference does not belong to this sink")
        }
        let mut created = self
            .created
            .lock()
            .map_err(|_| anyhow::anyhow!("credential file transaction is unavailable"))?;
        let Some(expected) = created.as_ref() else {
            return Ok(());
        };
        let metadata = std::fs::metadata(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.dev() != expected.device || metadata.ino() != expected.inode {
                anyhow::bail!("credential file changed during setup")
            }
        }
        #[cfg(not(unix))]
        let _ = (metadata, expected);
        std::fs::remove_file(&self.path)?;
        *created = None;
        Ok(())
    }
}

pub struct CredentialConfig {
    pub api_key: Option<SecretString>,
    pub command: Option<String>,
    pub file: Option<PathBuf>,
    pub platform: Option<(String, String)>,
    pub command_timeout: Duration,
}

impl fmt::Debug for CredentialConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialConfig")
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("command", &self.command.as_ref().map(|_| "[configured]"))
            .field("file", &self.file)
            .field("platform", &self.platform)
            .field("command_timeout", &self.command_timeout)
            .finish()
    }
}

pub async fn load_credential<B: KeyringBackend>(
    config: &CredentialConfig,
    backend: &B,
) -> anyhow::Result<Option<SecretString>> {
    if let Some(value) = config
        .api_key
        .as_ref()
        .filter(|value| !value.expose_secret().trim().is_empty())
    {
        return Ok(Some(value.clone()));
    }
    if let Some(command) = config.command.as_deref().filter(|value| present(Some(value))) {
        return load_command_credential(command, config.command_timeout)
            .await
            .map(Some);
    }
    if let Some(path) = config.file.as_ref() {
        let value = tokio::fs::read_to_string(path)
            .await
            .map_err(|_| anyhow::anyhow!("credential file could not be read"))?;
        let value = value.trim();
        if value.is_empty() {
            anyhow::bail!("credential file is empty")
        }
        return Ok(Some(SecretString::new(value.into())));
    }
    if let Some((service, account)) = config.platform.as_ref() {
        return backend.get_password(service, account);
    }
    Ok(None)
}

async fn load_command_credential(
    retrieval_command: &str,
    command_timeout: Duration,
) -> anyhow::Result<SecretString> {
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", retrieval_command]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", retrieval_command]);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(command_timeout, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("credential command timed out"))?
        .map_err(|_| anyhow::anyhow!("credential command could not be started"))?;
    if !output.status.success() {
        anyhow::bail!("credential command failed")
    }
    let output = String::from_utf8(output.stdout)
        .map_err(|_| anyhow::anyhow!("credential command returned invalid text"))?;
    let value = output.lines().next().unwrap_or_default().trim();
    if value.is_empty() {
        anyhow::bail!("credential command returned no credential")
    }
    Ok(SecretString::new(value.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Environment,
    Command,
    File,
    Platform,
}

pub fn choose_source(
    api_key: Option<&str>,
    command: Option<&str>,
    file: Option<&str>,
    platform: bool,
) -> Option<CredentialSource> {
    if present(api_key) {
        Some(CredentialSource::Environment)
    } else if present(command) {
        Some(CredentialSource::Command)
    } else if present(file) {
        Some(CredentialSource::File)
    } else if platform {
        Some(CredentialSource::Platform)
    } else {
        None
    }
}

fn present(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}
