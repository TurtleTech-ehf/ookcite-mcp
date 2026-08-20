use std::fmt;

use base64::Engine as _;
use rand::RngCore as _;
use secrecy::ExposeSecret as _;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::credentials::{CredentialReference, CredentialSink};

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
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
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
        expected_state: &str,
        timeout: std::time::Duration,
    ) -> Result<String, ConnectError> {
        let (mut stream, peer) = tokio::time::timeout(timeout, self.listener.accept())
            .await
            .map_err(|_| ConnectError::Expired)?
            .map_err(|_| ConnectError::Unavailable)?;
        if !peer.ip().is_loopback() {
            return Err(ConnectError::InvalidResponse);
        }

        use tokio::io::AsyncReadExt as _;
        let mut request = Vec::with_capacity(1024);
        loop {
            let mut chunk = [0_u8; 1024];
            let read = tokio::time::timeout(timeout, stream.read(&mut chunk))
                .await
                .map_err(|_| ConnectError::Expired)?
                .map_err(|_| ConnectError::InvalidResponse)?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if request.len() > 8192 {
                return Err(ConnectError::InvalidResponse);
            }
        }
        let request = std::str::from_utf8(&request).map_err(|_| ConnectError::InvalidResponse)?;
        let first_line = request
            .lines()
            .next()
            .ok_or(ConnectError::InvalidResponse)?;
        let mut parts = first_line.split_whitespace();
        if parts.next() != Some("GET") {
            return Err(ConnectError::InvalidResponse);
        }
        let target = parts.next().ok_or(ConnectError::InvalidResponse)?;
        let url = url::Url::parse(&format!("http://127.0.0.1{target}"))
            .map_err(|_| ConnectError::InvalidResponse)?;
        if url.path() != "/callback" {
            return Err(ConnectError::InvalidResponse);
        }
        let parameter = |name: &str| {
            url.query_pairs()
                .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
        };
        let received_state = parameter("state").ok_or(ConnectError::InvalidState)?;
        if !callback_state_matches(expected_state, &received_state) {
            let _ = write_callback_response(&mut stream, "400 Bad Request", "").await;
            return Err(ConnectError::InvalidState);
        }
        if let Some(error) = parameter("error") {
            let _ = write_callback_response(
                &mut stream,
                "200 OK",
                "Connection declined. Return to terminal.",
            )
            .await;
            return Err(match error.as_str() {
                "access_denied" => ConnectError::Denied,
                "cancelled" => ConnectError::Cancelled,
                _ => ConnectError::InvalidResponse,
            });
        }
        let code = parameter("code").ok_or(ConnectError::InvalidResponse)?;
        write_callback_response(
            &mut stream,
            "200 OK",
            "OokCite connected. Return to your terminal.",
        )
        .await?;
        Ok(code)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMode {
    Browser,
    Device,
}

pub fn open_browser_or_device(opener: &impl BrowserOpener, url: &str) -> ConnectMode {
    if opener.open(url).is_ok() {
        ConnectMode::Browser
    } else {
        ConnectMode::Device
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
        request: StartBrowserRequest<'_>,
    ) -> Result<StartBrowserResponse, ConnectError> {
        decode_response(
            self.client
                .post(self.endpoint("/mcp/ookcite/authorize"))
                .json(&request)
                .send()
                .await
                .map_err(|_| ConnectError::Unavailable)?,
        )
        .await
    }

    pub async fn start_device(
        &self,
        request: StartDeviceRequest<'_>,
    ) -> Result<StartDeviceResponse, ConnectError> {
        decode_response(
            self.client
                .post(self.endpoint("/mcp/ookcite/device"))
                .json(&request)
                .send()
                .await
                .map_err(|_| ConnectError::Unavailable)?,
        )
        .await
    }

    pub async fn poll_device(
        &self,
        device_code: &str,
        state: &str,
    ) -> Result<DevicePoll, ConnectError> {
        #[derive(Serialize)]
        struct Request<'a> {
            device_code: &'a str,
            state: &'a str,
        }
        #[derive(Deserialize)]
        struct Response {
            status: String,
            code: Option<String>,
        }
        let response: Response = decode_response(
            self.client
                .post(self.endpoint("/mcp/ookcite/device/poll"))
                .json(&Request { device_code, state })
                .send()
                .await
                .map_err(|_| ConnectError::Unavailable)?,
        )
        .await?;
        match response.status.as_str() {
            "pending" => Ok(DevicePoll::Pending),
            "authorization_code" => response
                .code
                .map(DevicePoll::AuthorizationCode)
                .ok_or(ConnectError::InvalidResponse),
            _ => Err(ConnectError::InvalidResponse),
        }
    }

    pub async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
    ) -> Result<ExchangeResult, ConnectError> {
        #[derive(Serialize)]
        struct Request<'a> {
            code: &'a str,
            verifier: &'a str,
            state: &'a str,
        }
        #[derive(Deserialize)]
        struct Response {
            api_key: String,
            installation_receipt: String,
            plan: String,
        }
        let response: Response = decode_response(
            self.client
                .post(self.endpoint("/mcp/ookcite/exchange"))
                .json(&Request {
                    code,
                    verifier,
                    state,
                })
                .send()
                .await
                .map_err(|_| ConnectError::Unavailable)?,
        )
        .await?;
        Ok(ExchangeResult {
            credential: SecretString::new(response.api_key),
            installation_receipt: response.installation_receipt,
            plan: response.plan,
        })
    }

    pub async fn redeem_receipt(&self, receipt: &str) -> Result<(), ConnectError> {
        let response = self
            .client
            .post(self.endpoint("/mcp/ookcite/receipt"))
            .json(&serde_json::json!({ "receipt": receipt }))
            .send()
            .await
            .map_err(|_| ConnectError::Unavailable)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(response_error(response).await)
        }
    }

    pub async fn cancel(&self, authorization_id: &str, state: &str) -> Result<(), ConnectError> {
        let response = self
            .client
            .post(self.endpoint(&format!(
                "/mcp/ookcite/authorizations/{authorization_id}/cancel"
            )))
            .json(&serde_json::json!({ "state": state }))
            .send()
            .await
            .map_err(|_| ConnectError::Unavailable)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(response_error(response).await)
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }
}

pub async fn verify_readiness(
    api_base: &str,
    credential: &SecretString,
) -> Result<Readiness, ConnectError> {
    decode_response(
        reqwest::Client::new()
            .get(format!("{}/api/v1/me", api_base.trim_end_matches('/')))
            .header("origin", "https://ookcite.turtletech.us")
            .bearer_auth(credential.expose_secret())
            .send()
            .await
            .map_err(|_| ConnectError::Unavailable)?,
    )
    .await
}

pub fn device_poll_delay(interval_seconds: u64) -> std::time::Duration {
    std::time::Duration::from_secs(interval_seconds)
}

pub async fn poll_device_until_authorized(
    client: &DashboardClient,
    started: &StartDeviceResponse,
    state: &str,
) -> Result<String, ConnectError> {
    let deadline = tokio::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(started.expires_in))
        .ok_or(ConnectError::Expired)?;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(ConnectError::Expired);
        }
        match client.poll_device(&started.device_code, state).await? {
            DevicePoll::AuthorizationCode(code) => return Ok(code),
            DevicePoll::Pending => {
                let delay = device_poll_delay(started.interval);
                if tokio::time::Instant::now() + delay >= deadline {
                    return Err(ConnectError::Expired);
                }
                tokio::time::sleep(delay).await;
            }
        }
    }
}

pub fn connection_config_env(
    reference: &CredentialReference,
    journey_id: &str,
) -> Vec<(String, String)> {
    let mut environment = reference.config_env();
    environment.push(("OOKCITE_JOURNEY_ID".into(), journey_id.into()));
    environment
}

pub async fn finalize_installation<S, F>(
    dashboard: &DashboardClient,
    api_base: &str,
    exchange: &ExchangeResult,
    sink: &S,
    configure: F,
) -> anyhow::Result<CredentialReference>
where
    S: CredentialSink + ?Sized,
    F: FnOnce(&CredentialReference) -> anyhow::Result<()>,
{
    let reference = sink.store(&exchange.credential)?;
    let readiness = match verify_readiness(api_base, &exchange.credential).await {
        Ok(readiness) if readiness.authenticated => readiness,
        Ok(_) => {
            let _ = sink.cleanup(&reference);
            anyhow::bail!("connected credential was not accepted")
        }
        Err(_) => {
            let _ = sink.cleanup(&reference);
            anyhow::bail!("connected credential readiness check failed")
        }
    };
    let _ = readiness;
    if configure(&reference).is_err() {
        let _ = sink.cleanup(&reference);
        anyhow::bail!("client configuration failed")
    }
    dashboard
        .redeem_receipt(&exchange.installation_receipt)
        .await
        .map_err(|_| anyhow::anyhow!("credential installation receipt could not be recorded"))?;
    Ok(reference)
}

pub fn generate_pkce() -> Pkce {
    Pkce::from_verifier(random_token())
}

pub fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn random_journey_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn decode_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, ConnectError> {
    if !response.status().is_success() {
        return Err(response_error(response).await);
    }
    response
        .json::<T>()
        .await
        .map_err(|_| ConnectError::InvalidResponse)
}

async fn response_error(response: reqwest::Response) -> ConnectError {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<String>,
    }
    let status = response.status();
    let code = response
        .json::<ErrorBody>()
        .await
        .ok()
        .and_then(|body| body.error);
    match code.as_deref() {
        Some("invalid_state") => ConnectError::InvalidState,
        Some("expired") => ConnectError::Expired,
        Some("access_denied") => ConnectError::Denied,
        Some("cancelled") => ConnectError::Cancelled,
        Some("already_consumed") => ConnectError::AlreadyConsumed,
        _ if status == reqwest::StatusCode::GONE => ConnectError::Expired,
        _ if status == reqwest::StatusCode::FORBIDDEN => ConnectError::Denied,
        _ if status == reqwest::StatusCode::CONFLICT => ConnectError::AlreadyConsumed,
        _ => ConnectError::Unavailable,
    }
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> Result<(), ConnectError> {
    use tokio::io::AsyncWriteExt as _;
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|_| ConnectError::Unavailable)
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
