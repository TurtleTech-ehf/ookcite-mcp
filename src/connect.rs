use std::fmt;

use base64::Engine as _;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
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

    pub async fn wait_for_callback(
        &self,
        _expected_state: &str,
        _timeout: std::time::Duration,
    ) -> Result<String, ConnectError> {
        Err(ConnectError::Unavailable)
    }
}

pub trait BrowserOpener {
    fn open(&self, url: &str) -> anyhow::Result<()>;
}

pub struct SystemBrowser;

impl BrowserOpener for SystemBrowser {
    fn open(&self, url: &str) -> anyhow::Result<()> {
        webbrowser::open(url)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    InvalidState,
    Expired,
    Denied,
    Cancelled,
    AlreadyConsumed,
    Unavailable,
    InvalidResponse,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidState => "connection state did not match",
            Self::Expired => "connection request expired",
            Self::Denied => "connection request was denied",
            Self::Cancelled => "connection request was cancelled",
            Self::AlreadyConsumed => "connection request was already used",
            Self::Unavailable => "connection service is unavailable",
            Self::InvalidResponse => "connection service returned an invalid response",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ConnectError {}

#[derive(Debug, Clone, Serialize)]
pub struct StartBrowserRequest<'a> {
    pub journey_id: &'a str,
    pub code_challenge: &'a str,
    pub code_challenge_method: &'static str,
    pub state: &'a str,
    pub callback_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartBrowserResponse {
    pub authorization_id: String,
    pub authorization_url: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartDeviceRequest<'a> {
    pub journey_id: &'a str,
    pub code_challenge: &'a str,
    pub code_challenge_method: &'static str,
    pub state: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartDeviceResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePoll {
    Pending,
    AuthorizationCode(String),
}

pub struct ExchangeResult {
    pub credential: SecretString,
    pub installation_receipt: String,
    pub plan: String,
}

impl fmt::Debug for ExchangeResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExchangeResult")
            .field("credential", &"[redacted]")
            .field("installation_receipt", &"[redacted]")
            .field("plan", &self.plan)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Readiness {
    pub authenticated: bool,
    pub plan: String,
    pub lookups_remaining: u32,
    pub lookups_limit: u32,
}

#[derive(Clone)]
pub struct DashboardClient {
    base_url: String,
    client: reqwest::Client,
}

impl DashboardClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn start_browser(
        &self,
        _request: StartBrowserRequest<'_>,
    ) -> Result<StartBrowserResponse, ConnectError> {
        let _ = (&self.base_url, &self.client);
        Err(ConnectError::Unavailable)
    }

    pub async fn start_device(
        &self,
        _request: StartDeviceRequest<'_>,
    ) -> Result<StartDeviceResponse, ConnectError> {
        Err(ConnectError::Unavailable)
    }

    pub async fn poll_device(
        &self,
        _device_code: &str,
        _state: &str,
    ) -> Result<DevicePoll, ConnectError> {
        Err(ConnectError::Unavailable)
    }

    pub async fn exchange(
        &self,
        _code: &str,
        _verifier: &str,
        _state: &str,
    ) -> Result<ExchangeResult, ConnectError> {
        Err(ConnectError::Unavailable)
    }

    pub async fn redeem_receipt(&self, _receipt: &str) -> Result<(), ConnectError> {
        Err(ConnectError::Unavailable)
    }
}

pub async fn verify_readiness(
    _api_base: &str,
    _credential: &SecretString,
) -> Result<Readiness, ConnectError> {
    Err(ConnectError::Unavailable)
}

pub fn generate_pkce() -> Pkce {
    Pkce::from_verifier("")
}

pub fn random_token() -> String {
    String::new()
}

pub fn random_journey_id() -> String {
    String::new()
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
