//! Credential for one OokCite API call.
//!
//! Stdio reads `OOKCITE_API_KEY` from the process. Streamable HTTP sets a
//! task-local for the request and does not fall through to that variable, so a
//! key loaded on the server cannot authenticate someone else's request.

use std::future::Future;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

tokio::task_local! {
    static HTTP_BEARER: Option<String>;
}

pub fn process_api_key() -> Option<String> {
    std::env::var("OOKCITE_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn in_http_scope() -> bool {
    HTTP_BEARER.try_with(|_| ()).is_ok()
}

pub fn has_api_key() -> bool {
    api_key().is_some()
}

/// Key for the OokCite API on this task. Inside an HTTP request the value is
/// only the inbound credential, including "absent".
pub fn api_key() -> Option<String> {
    match HTTP_BEARER.try_with(|key| key.clone()) {
        Ok(key) => key.filter(|value| !value.is_empty()),
        Err(_) => process_api_key(),
    }
}

pub async fn with_http_bearer<T>(key: Option<String>, fut: impl Future<Output = T>) -> T {
    HTTP_BEARER.scope(key, fut).await
}

pub fn apply_bearer(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let Some(key) = api_key() else {
        return builder;
    };
    let Ok(mut value) = format!("Bearer {key}").parse::<HeaderValue>() else {
        return builder;
    };
    value.set_sensitive(true);
    builder.header(AUTHORIZATION, value)
}

pub fn auth_required_text() -> &'static str {
    if in_http_scope() {
        if std::env::var("OOKCITE_MCP_HTTP_AUTH")
            .ok()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("oauth"))
        {
            "Sign in is required."
        } else {
            "Authentication required. Send Authorization: Bearer with an OokCite API key."
        }
    } else {
        "Authentication required. Set OOKCITE_API_KEY."
    }
}

/// An API key is short. A sign-in access token is a JWT and is longer
/// than 512 characters, so the bearer cap has to admit one header line
/// without treating the token as an API key.
const BEARER_MAX_LEN: usize = 8192;

/// Printable ASCII token, no whitespace, short enough for one header.
pub fn sanitize_api_key(raw: &str) -> Option<String> {
    let key = raw.trim();
    if key.is_empty() || key.len() > BEARER_MAX_LEN {
        return None;
    }
    if key
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || !c.is_ascii())
    {
        return None;
    }
    HeaderValue::from_str(&format!("Bearer {key}")).ok()?;
    Some(key.to_string())
}

/// `Authorization: Bearer` or `X-Api-Key`. Other schemes are not a key.
pub fn inbound_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION) {
        let text = value.to_str().ok()?.trim();
        let rest = text
            .strip_prefix("Bearer ")
            .or_else(|| text.strip_prefix("bearer "))?;
        return sanitize_api_key(rest);
    }
    if let Some(value) = headers.get("x-api-key") {
        return sanitize_api_key(value.to_str().ok()?);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpAuthMode {
    /// A local copy may allow requests with no credential.
    None,
    /// A local copy may require an API key. The hosted endpoint does not.
    Bearer,
    /// Hosted endpoint: a short-lived sign-in token, not an API key.
    Oauth,
}

pub fn parse_auth_mode(raw: &str) -> Result<HttpAuthMode, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "bearer" | "api-key" | "apikey" => Ok(HttpAuthMode::Bearer),
        "none" | "anonymous" => Ok(HttpAuthMode::None),
        "oauth" => Ok(HttpAuthMode::Oauth),
        other => Err(format!(
            "OOKCITE_MCP_HTTP_AUTH must be oauth, bearer, or none, got {other}"
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDeny {
    Unauthorized,
    ForbiddenHost,
    ForbiddenOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub auth: HttpAuthMode,
    pub origins: Vec<String>,
    pub hosts: Vec<String>,
    pub path: String,
}

impl Gate {
    pub fn loopback_hosts() -> Vec<String> {
        ["localhost", "127.0.0.1", "::1"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    pub fn from_env() -> Result<Self, String> {
        let auth = parse_auth_mode(
            &std::env::var("OOKCITE_MCP_HTTP_AUTH").unwrap_or_else(|_| "bearer".into()),
        )?;
        let mut hosts = Self::loopback_hosts();
        if let Ok(extra) = std::env::var("OOKCITE_MCP_ALLOWED_HOSTS") {
            hosts.extend(split_list(&extra));
        }
        let origins = std::env::var("OOKCITE_MCP_ALLOWED_ORIGINS")
            .ok()
            .map(|value| split_list(&value))
            .unwrap_or_default();
        let path =
            normalize_path(&std::env::var("OOKCITE_MCP_PATH").unwrap_or_else(|_| "/mcp".into()));
        Ok(Self {
            auth,
            origins,
            hosts,
            path,
        })
    }

    pub fn decide(&self, headers: &HeaderMap) -> Result<(), GateDeny> {
        let host = headers
            .get(reqwest::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !host_allowed(host, &self.hosts) {
            return Err(GateDeny::ForbiddenHost);
        }
        let origin = headers
            .get(reqwest::header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        if !origin_allowed(origin, &self.origins) {
            return Err(GateDeny::ForbiddenOrigin);
        }
        match self.auth {
            HttpAuthMode::None => Ok(()),
            HttpAuthMode::Bearer => {
                if inbound_api_key(headers).is_none() {
                    Err(GateDeny::Unauthorized)
                } else {
                    Ok(())
                }
            }
            HttpAuthMode::Oauth => match inbound_api_key(headers).as_deref() {
                Some(token) if !token.starts_with("ookc_") => Ok(()),
                _ => Err(GateDeny::Unauthorized),
            },
        }
    }
}

pub fn normalize_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/mcp".into();
    }
    let with_slash = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    with_slash.trim_end_matches('/').to_string()
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_ascii_lowercase())
        .collect()
}

pub fn host_name(host_header: &str) -> String {
    let host = host_header.trim();
    if let Some(rest) = host.strip_prefix('[') {
        if let Some((inside, _)) = rest.split_once(']') {
            return inside.to_ascii_lowercase();
        }
    }
    host.split_once(':')
        .map(|(name, _)| name)
        .unwrap_or(host)
        .to_ascii_lowercase()
}

pub fn host_allowed(host_header: &str, allow: &[String]) -> bool {
    if host_header.is_empty() {
        return false;
    }
    let name = host_name(host_header);
    allow.iter().any(|item| item.eq_ignore_ascii_case(&name))
}

/// A missing Origin is a non-browser client. A present Origin must be listed.
pub fn origin_allowed(origin: Option<&str>, allow: &[String]) -> bool {
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    allow.iter().any(|item| item.eq_ignore_ascii_case(origin))
}

pub fn bind_from_args(args: &[String]) -> String {
    args.windows(2)
        .find(|pair| pair[0] == "--bind")
        .map(|pair| pair[1].clone())
        .or_else(|| std::env::var("OOKCITE_MCP_BIND").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1:8080".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_and_api_key_headers_parse() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer ookc_abc".parse().unwrap());
        assert_eq!(inbound_api_key(&headers).as_deref(), Some("ookc_abc"));

        let mut api_key = HeaderMap::new();
        api_key.insert("x-api-key", "ookc_xyz".parse().unwrap());
        assert_eq!(inbound_api_key(&api_key).as_deref(), Some("ookc_xyz"));

        let mut basic = HeaderMap::new();
        basic.insert(AUTHORIZATION, "Basic og==".parse().unwrap());
        assert!(inbound_api_key(&basic).is_none());
    }

    #[test]
    fn sanitize_rejects_header_injection() {
        assert!(sanitize_api_key("ookc_ok").is_some());
        assert!(sanitize_api_key("ookc\r\nSet-Cookie: x").is_none());
        assert!(sanitize_api_key("has space").is_none());
        assert!(sanitize_api_key("").is_none());
    }

    #[test]
    fn gate_requires_bearer_and_checks_host_and_origin() {
        let gate = Gate {
            auth: HttpAuthMode::Bearer,
            origins: vec!["https://chat.example".into()],
            hosts: Gate::loopback_hosts(),
            path: "/mcp".into(),
        };
        let mut ok = HeaderMap::new();
        ok.insert(reqwest::header::HOST, "127.0.0.1:8080".parse().unwrap());
        ok.insert(AUTHORIZATION, "Bearer ookc_abc".parse().unwrap());
        assert!(gate.decide(&ok).is_ok());

        let mut missing = ok.clone();
        missing.remove(AUTHORIZATION);
        assert_eq!(gate.decide(&missing), Err(GateDeny::Unauthorized));

        let mut evil_host = ok.clone();
        evil_host.insert(reqwest::header::HOST, "evil.example".parse().unwrap());
        assert_eq!(gate.decide(&evil_host), Err(GateDeny::ForbiddenHost));

        let mut evil_origin = ok.clone();
        evil_origin.insert(
            reqwest::header::ORIGIN,
            "https://evil.example".parse().unwrap(),
        );
        assert_eq!(gate.decide(&evil_origin), Err(GateDeny::ForbiddenOrigin));

        let mut listed = ok.clone();
        listed.insert(
            reqwest::header::ORIGIN,
            "https://chat.example".parse().unwrap(),
        );
        assert!(gate.decide(&listed).is_ok());
    }

    #[test]
    fn none_allows_a_request_without_a_key() {
        let gate = Gate {
            auth: HttpAuthMode::None,
            origins: Vec::new(),
            hosts: Gate::loopback_hosts(),
            path: "/mcp".into(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::HOST, "localhost".parse().unwrap());
        assert!(gate.decide(&headers).is_ok());
    }

    #[test]
    fn oauth_mode_is_selected_for_sign_in() {
        assert_eq!(parse_auth_mode("oauth").unwrap(), HttpAuthMode::Oauth);
        assert_eq!(parse_auth_mode("api-key").unwrap(), HttpAuthMode::Bearer);
        assert_eq!(parse_auth_mode("none").unwrap(), HttpAuthMode::None);
    }

    #[test]
    fn oauth_gate_rejects_a_pasted_api_key() {
        let gate = Gate {
            auth: HttpAuthMode::Oauth,
            origins: Vec::new(),
            hosts: Gate::loopback_hosts(),
            path: "/mcp".into(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::HOST, "localhost".parse().unwrap());
        headers.insert(AUTHORIZATION, "Bearer ookc_live".parse().unwrap());
        assert_eq!(gate.decide(&headers), Err(GateDeny::Unauthorized));

        headers.insert(AUTHORIZATION, "Bearer a.b.c".parse().unwrap());
        assert!(gate.decide(&headers).is_ok());

        let long = format!("Bearer {}", "a".repeat(600));
        headers.insert(AUTHORIZATION, long.parse().unwrap());
        assert!(
            gate.decide(&headers).is_ok(),
            "a sign-in token longer than an API key must reach verification"
        );
        let huge = format!("Bearer {}", "a".repeat(9000));
        headers.insert(AUTHORIZATION, huge.parse().unwrap());
        assert_eq!(gate.decide(&headers), Err(GateDeny::Unauthorized));
    }

    #[tokio::test]
    async fn http_scope_uses_the_inbound_key_only() {
        let seen = with_http_bearer(Some("ookc_header".into()), async { api_key() }).await;
        assert_eq!(seen.as_deref(), Some("ookc_header"));
        let absent = with_http_bearer(None, async { api_key() }).await;
        assert!(absent.is_none());
        assert!(!in_http_scope());
    }
}
