//! HTTP error formatting and classification for tool responses.

use std::fmt;
use std::ops::Deref;

use reqwest::header::RETRY_AFTER;
use reqwest::{Response, StatusCode};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResult, Content};

const MAX_ERROR_BODY_CHARS: usize = 120;
const MAX_RETRY_AFTER_CHARS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpFailure {
    kind: &'static str,
    http_status: Option<u16>,
    message: String,
    retry_after: Option<String>,
}

impl HttpFailure {
    pub async fn from_response(resp: Response, subject: Option<&str>) -> Self {
        let failure = ResponseFailure::read(resp).await;
        let (kind, label) = classify_status(failure.status);
        let message = match subject {
            Some(subject) => format!("{label} {subject} : {}", failure.detail),
            None => format!("{label}: {}", failure.detail),
        };
        let message = append_retry_after(message, failure.retry_after.as_deref());

        Self {
            kind,
            http_status: Some(failure.status.as_u16()),
            message,
            retry_after: failure.retry_after,
        }
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: "transport_error",
            http_status: None,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn into_message(self) -> String {
        self.message
    }

    fn structured(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "http_status": self.http_status,
            "message": self.message,
            "retry_after": self.retry_after,
        })
    }
}

#[derive(Clone, Debug, Eq)]
pub enum ToolResponse {
    Success(String),
    Failure(HttpFailure),
}

impl ToolResponse {
    pub fn success(message: impl Into<String>) -> Self {
        Self::Success(message.into())
    }

    pub fn failure(failure: HttpFailure) -> Self {
        Self::Failure(failure)
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Success(message) => message,
            Self::Failure(failure) => failure.message(),
        }
    }
}

impl From<String> for ToolResponse {
    fn from(message: String) -> Self {
        Self::success(message)
    }
}

impl From<&str> for ToolResponse {
    fn from(message: &str) -> Self {
        Self::success(message)
    }
}

impl From<HttpFailure> for ToolResponse {
    fn from(failure: HttpFailure) -> Self {
        Self::failure(failure)
    }
}

impl Deref for ToolResponse {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.text()
    }
}

impl fmt::Display for ToolResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.text())
    }
}

impl PartialEq for ToolResponse {
    fn eq(&self, other: &Self) -> bool {
        self.text() == other.text()
    }
}

impl PartialEq<&str> for ToolResponse {
    fn eq(&self, other: &&str) -> bool {
        self.text() == *other
    }
}

impl IntoCallToolResult for ToolResponse {
    fn into_call_tool_result(self) -> Result<CallToolResult, rmcp::ErrorData> {
        match self {
            Self::Success(message) => Ok(CallToolResult::success(vec![Content::text(message)])),
            Self::Failure(failure) => {
                let structured = failure.structured();
                let mut result = CallToolResult::error(vec![Content::text(failure.into_message())]);
                result.structured_content = Some(structured);
                Ok(result)
            }
        }
    }
}

struct ResponseFailure {
    status: StatusCode,
    detail: String,
    retry_after: Option<String>,
}

impl ResponseFailure {
    async fn read(resp: Response) -> Self {
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.chars().take(MAX_RETRY_AFTER_CHARS).collect());
        let body = resp.text().await.unwrap_or_default();

        Self {
            status,
            detail: format_error_detail(status, &body),
            retry_after,
        }
    }
}

fn classify_status(status: StatusCode) -> (&'static str, &'static str) {
    match status.as_u16() {
        429 => ("rate_limited", "RATE LIMITED"),
        401 | 403 => ("access_denied", "ACCESS DENIED"),
        408 | 504 => ("timeout", "TIMEOUT"),
        500..=599 => ("temporary_error", "TEMPORARY ERROR"),
        404 => ("not_found", "INVALID"),
        400..=499 => ("client_error", "CLIENT ERROR"),
        _ => ("http_error", "HTTP ERROR"),
    }
}

fn append_retry_after(mut message: String, retry_after: Option<&str>) -> String {
    if let Some(retry_after) = retry_after {
        message.push_str(" (Retry-After: ");
        message.push_str(retry_after);
        message.push(')');
    }
    message
}

fn format_error_detail(status: StatusCode, body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(message) = json["message"].as_str() {
            return format!("{status}: {message}");
        }
    }

    if body.is_empty() {
        format!("{status}")
    } else if body.chars().count() > MAX_ERROR_BODY_CHARS {
        let body: String = body.chars().take(MAX_ERROR_BODY_CHARS).collect();
        format!("{status}: {body}")
    } else {
        format!("{status}: {body}")
    }
}

/// Extract a useful error message from a failed HTTP response.
pub async fn error_detail(resp: reqwest::Response) -> String {
    ResponseFailure::read(resp).await.detail
}

pub async fn lookup_doi_failure(resp: reqwest::Response, doi: &str) -> HttpFailure {
    HttpFailure::from_response(resp, Some(doi)).await
}

pub async fn classify_lookup_doi_failure(resp: reqwest::Response, doi: &str) -> String {
    if resp.status().as_u16() == 429 {
        format!("RATE LIMITED {doi} : {}", error_detail(resp).await)
    } else if matches!(resp.status().as_u16(), 401 | 403) {
        format!("ACCESS DENIED {doi} : {}", error_detail(resp).await)
    } else if resp.status().is_server_error() {
        format!("TEMPORARY ERROR {doi} : {}", error_detail(resp).await)
    } else if resp.status().is_client_error() && resp.status().as_u16() != 404 {
        format!("CLIENT ERROR {doi} : {}", error_detail(resp).await)
    } else {
        format!("INVALID {doi} : HTTP {}", resp.status())
    }
}

pub async fn classify_collection_create_failure(resp: reqwest::Response, name: &str) -> String {
    let detail = error_detail(resp).await;
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("collection limit reached") {
        format!(
            "Collection '{name}' could not be created: {detail}. Use an existing collection, upgrade your plan, or purchase additional collections."
        )
    } else if lowered.contains("plan_required") || lowered.contains("requires") {
        format!(
            "Collection '{name}' could not be created: {detail}. This workflow may require a paid plan or additional collection capacity."
        )
    } else {
        format!("Failed to create collection '{name}': {detail}")
    }
}
