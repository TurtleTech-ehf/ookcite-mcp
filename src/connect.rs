use std::fmt;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

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
        let verifier = verifier.into();
        Self {
            challenge: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes())),
            verifier,
        }
    }
}

pub fn callback_state_matches(expected: &str, received: &str) -> bool {
    if expected.len() != received.len() {
        return false;
    }
    expected
        .bytes()
        .zip(received.bytes())
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

pub struct LoopbackListener {
    listener: tokio::net::TcpListener,
}

impl LoopbackListener {
    pub async fn bind(port: u16) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        Ok(Self { listener })
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
