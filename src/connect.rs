use std::fmt;

#[derive(Clone)]
pub struct ConnectSecrets {
    pub state: String,
    pub verifier: String,
    pub authorization_code: Option<String>,
}

impl fmt::Debug for ConnectSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectSecrets")
            .field("state", &"[redacted]")
            .field("verifier", &"[redacted]")
            .field(
                "authorization_code",
                &self.authorization_code.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        Self {
            verifier: verifier.into(),
            challenge: String::new(),
        }
    }
}

pub fn callback_state_matches(_expected: &str, _received: &str) -> bool {
    false
}

pub struct LoopbackListener {
    listener: tokio::net::TcpListener,
}

impl LoopbackListener {
    pub async fn bind(_port: u16) -> anyhow::Result<Self> {
        anyhow::bail!("loopback callback is unavailable")
    }

    pub fn local_addr(&self) -> anyhow::Result<std::net::SocketAddr> {
        Ok(self.listener.local_addr()?)
    }
}

pub fn redact_diagnostic(message: &str, secrets: &ConnectSecrets) -> String {
    let mut redacted = message.to_string();
    for secret in [
        Some(secrets.state.as_str()),
        Some(secrets.verifier.as_str()),
        secrets.authorization_code.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        redacted = redacted.replace(secret, "[redacted]");
    }
    redacted
}
