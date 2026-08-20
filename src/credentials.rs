use std::fmt;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;

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
    pub path: PathBuf,
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
        Ok(CredentialReference::File {
            path: self.path.clone(),
        })
    }
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
