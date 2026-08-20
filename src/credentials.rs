use std::fmt;
use std::path::PathBuf;

use secrecy::SecretString;

#[derive(Clone, PartialEq, Eq)]
pub enum CredentialReference {
    Platform { service: String, account: String },
    Command { retrieve_command: String },
    File { path: PathBuf },
}

impl CredentialReference {
    pub fn config_env(&self) -> Vec<(String, String)> {
        Vec::new()
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
    fn store(&self, _secret: &SecretString) -> anyhow::Result<CredentialReference> {
        anyhow::bail!("store command is unavailable")
    }
}

pub struct ProtectedFileSink {
    pub path: PathBuf,
}

impl CredentialSink for ProtectedFileSink {
    fn store(&self, _secret: &SecretString) -> anyhow::Result<CredentialReference> {
        anyhow::bail!("protected file storage is unavailable")
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
    _api_key: Option<&str>,
    _command: Option<&str>,
    _file: Option<&str>,
    _platform: bool,
) -> Option<CredentialSource> {
    None
}
