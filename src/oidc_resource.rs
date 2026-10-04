//! OAuth resource server for the remote MCP endpoint.
//!
//! Callers sign in and send a short-lived access token. This module checks
//! that token against each configured issuer's published keys. It does not
//! accept an API key, and it does not embed an issuer address.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

const JWKS_TTL: Duration = Duration::from_secs(600);

/// One extra authorization server, paired with the audience that server writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedClient {
    pub issuer: String,
    pub audience: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcPolicy {
    pub issuer: String,
    pub audience: String,
    /// Further clients whose tokens are accepted and advertised.
    pub extra: Vec<TrustedClient>,
    pub scope: String,
    pub resource: String,
}

impl OidcPolicy {
    pub fn from_env() -> Result<Self, String> {
        let issuer = required_https_url("OOKCITE_MCP_OIDC_ISSUER")?;
        // The audience is whatever the identity provider writes into the
        // token. That is often a client name, not a URL.
        let audience = required_token("OOKCITE_MCP_OIDC_AUDIENCE")?;
        let resource = match std::env::var("OOKCITE_MCP_RESOURCE") {
            Ok(value) if !value.trim().is_empty() => {
                require_https_url("OOKCITE_MCP_RESOURCE", value.trim())?
            }
            _ => audience.clone(),
        };
        let scope = std::env::var("OOKCITE_MCP_OIDC_SCOPE").unwrap_or_else(|_| "openid".into());
        let scope = scope.trim().to_string();
        if scope.is_empty() || scope.chars().any(|c| c.is_whitespace()) {
            return Err("OOKCITE_MCP_OIDC_SCOPE must be one scope token".into());
        }
        Ok(Self {
            issuer,
            audience,
            extra: extra_clients()?,
            scope,
            resource,
        })
    }

    pub fn authorization_servers(&self) -> Vec<String> {
        let mut servers = Vec::new();
        for client in &self.extra {
            push_unique(&mut servers, &client.issuer);
        }
        push_unique(&mut servers, &self.issuer);
        servers
    }

    /// Configured issuer that matches the token's `iss`, without a signature check.
    pub fn issuer_for_token(&self, token: &str) -> Option<String> {
        let raw = unverified_issuer(token)?;
        let key = raw.trim_end_matches('/');
        if self.issuer.trim_end_matches('/') == key {
            return Some(self.issuer.trim_end_matches('/').to_string());
        }
        self.extra
            .iter()
            .find(|client| client.issuer.trim_end_matches('/') == key)
            .map(|client| client.issuer.trim_end_matches('/').to_string())
    }

    pub fn audience_for(&self, issuer: &str) -> Option<&str> {
        let issuer = issuer.trim_end_matches('/');
        if self.issuer.trim_end_matches('/') == issuer {
            return Some(self.audience.as_str());
        }
        self.extra
            .iter()
            .find(|client| client.issuer.trim_end_matches('/') == issuer)
            .map(|client| client.audience.as_str())
    }

    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "resource": self.resource,
            "authorization_servers": self.authorization_servers(),
            "bearer_methods_supported": ["header"],
            "scopes_supported": [self.scope],
        })
    }

    pub fn metadata_url(&self) -> String {
        // RFC 9728 inserts the well-known segment between the host and the
        // resource path. A resource of https://host/mcp is discovered at
        // https://host/.well-known/oauth-protected-resource/mcp.
        let Ok(mut url) = reqwest::Url::parse(&self.resource) else {
            return format!(
                "{}/.well-known/oauth-protected-resource",
                self.resource.trim_end_matches('/')
            );
        };
        let path = url.path().trim_matches('/');
        let well_known = if path.is_empty() {
            "/.well-known/oauth-protected-resource".to_string()
        } else {
            format!("/.well-known/oauth-protected-resource/{path}")
        };
        url.set_path(&well_known);
        url.to_string()
    }

    pub fn challenge(&self) -> String {
        format!(
            "Bearer realm=\"ookcite\", resource_metadata=\"{}\"",
            self.metadata_url()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessSubject {
    pub subject: String,
    pub username: String,
}

#[derive(Debug, Deserialize)]
struct AccessClaims {
    sub: String,
    iat: u64,
    exp: u64,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifyFail {
    Rejected,
    UnknownKey,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn verify_access_token(
    token: &str,
    policy: &OidcPolicy,
    jwks: &JwkSet,
) -> Result<AccessSubject, VerifyFail> {
    let issuer = policy.issuer_for_token(token).ok_or(VerifyFail::Rejected)?;
    let audience = policy.audience_for(&issuer).ok_or(VerifyFail::Rejected)?;
    verify_access_token_with(token, &issuer, audience, &policy.scope, jwks)
}

fn verify_access_token_with(
    token: &str,
    issuer: &str,
    audience: &str,
    scope: &str,
    jwks: &JwkSet,
) -> Result<AccessSubject, VerifyFail> {
    if token.starts_with("ookc_") || token.chars().filter(|c| *c == '.').count() != 2 {
        return Err(VerifyFail::Rejected);
    }
    let header = decode_header(token).map_err(|_| VerifyFail::Rejected)?;
    if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
        return Err(VerifyFail::Rejected);
    }
    let kid = header.kid.as_deref().ok_or(VerifyFail::Rejected)?;
    let Some(jwk) = jwks.find(kid) else {
        return Err(VerifyFail::UnknownKey);
    };
    let key = DecodingKey::from_jwk(jwk).map_err(|_| VerifyFail::Rejected)?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    let data =
        decode::<AccessClaims>(token, &key, &validation).map_err(|_| VerifyFail::Rejected)?;
    if data.claims.exp.saturating_sub(data.claims.iat) > 3600 {
        return Err(VerifyFail::Rejected);
    }
    if !has_scope(data.claims.scope.as_deref(), scope) {
        return Err(VerifyFail::Rejected);
    }
    if data.claims.sub.is_empty() || data.claims.sub.len() > 256 {
        return Err(VerifyFail::Rejected);
    }
    let username = match data.claims.preferred_username.as_deref() {
        Some(name) if username_ok(name) => name.to_string(),
        _ => data.claims.sub.clone(),
    };
    if !username_ok(&username) {
        return Err(VerifyFail::Rejected);
    }
    Ok(AccessSubject {
        subject: data.claims.sub,
        username,
    })
}

fn has_scope(scope: Option<&str>, required: &str) -> bool {
    scope.unwrap_or("").split(' ').any(|item| item == required)
}

fn username_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub struct OidcVerifier {
    policy: OidcPolicy,
    http: reqwest::Client,
    cache: tokio::sync::RwLock<HashMap<String, CachedJwks>>,
    refresh: tokio::sync::Mutex<()>,
}

struct CachedJwks {
    set: Option<JwkSet>,
    userinfo: Option<String>,
    fetched: Instant,
    refreshed_at: Instant,
}

impl OidcVerifier {
    pub fn from_env() -> Result<Self, String> {
        Self::new(OidcPolicy::from_env()?)
    }

    pub fn new(policy: OidcPolicy) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self {
            policy,
            http,
            cache: tokio::sync::RwLock::new(HashMap::new()),
            refresh: tokio::sync::Mutex::new(()),
        })
    }

    pub fn policy(&self) -> &OidcPolicy {
        &self.policy
    }

    pub async fn verify(&self, token: &str) -> Result<AccessSubject, ()> {
        let Some(issuer) = self.policy.issuer_for_token(token) else {
            return Err(());
        };
        let Some(audience) = self.policy.audience_for(&issuer) else {
            return Err(());
        };
        let jwks = self.jwks_for(&issuer).await.map_err(|_| ())?;
        match verify_access_token_with(token, &issuer, audience, &self.policy.scope, &jwks) {
            Ok(subject) => self.with_account_name(token, &issuer, subject).await,
            Err(VerifyFail::Rejected) => Err(()),
            Err(VerifyFail::UnknownKey) => {
                let jwks = self
                    .refresh_for_unknown_key(&issuer)
                    .await
                    .map_err(|_| ())?;
                match verify_access_token_with(token, &issuer, audience, &self.policy.scope, &jwks)
                {
                    Ok(subject) => self.with_account_name(token, &issuer, subject).await,
                    Err(_) => Err(()),
                }
            }
        }
    }

    async fn with_account_name(
        &self,
        token: &str,
        issuer: &str,
        mut subject: AccessSubject,
    ) -> Result<AccessSubject, ()> {
        if subject.username != subject.subject {
            return Ok(subject);
        }
        let name = self.account_name(token, issuer).await.map_err(|_| ())?;
        subject.username = name;
        Ok(subject)
    }

    async fn account_name(&self, token: &str, issuer: &str) -> Result<String, String> {
        let url = self
            .cache
            .read()
            .await
            .get(issuer)
            .and_then(|cached| cached.userinfo.clone())
            .ok_or_else(|| "account name endpoint is not published".to_string())?;
        if !same_origin(&url, issuer) {
            return Err("account name endpoint is not on the issuer origin".into());
        }
        let doc = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|err| err.to_string())?
            .error_for_status()
            .map_err(|err| err.to_string())?
            .json::<AccountInfo>()
            .await
            .map_err(|err| err.to_string())?;
        let name = doc
            .preferred_username
            .as_deref()
            .map(account_local_name)
            .filter(|name| username_ok(name))
            .ok_or_else(|| "account name missing".to_string())?;
        Ok(name)
    }

    async fn refresh_for_unknown_key(&self, issuer: &str) -> Result<JwkSet, String> {
        let _hold = self.refresh.lock().await;
        if let Some(cached) = self.cache.read().await.get(issuer) {
            if cached.refreshed_at.elapsed() < Duration::from_secs(30) {
                return cached
                    .set
                    .clone()
                    .ok_or_else(|| "signing keys unavailable".into());
            }
        }
        self.fetch_keys(issuer).await
    }

    async fn jwks_for(&self, issuer: &str) -> Result<JwkSet, String> {
        let _hold = self.refresh.lock().await;
        if let Some(cached) = self.cache.read().await.get(issuer) {
            if cached.fetched.elapsed() < JWKS_TTL {
                if let Some(set) = &cached.set {
                    return Ok(set.clone());
                }
            }
            if cached.refreshed_at.elapsed() < Duration::from_secs(30) {
                return cached
                    .set
                    .clone()
                    .ok_or_else(|| "signing keys unavailable".into());
            }
        }
        self.fetch_keys(issuer).await
    }

    async fn fetch_keys(&self, issuer: &str) -> Result<JwkSet, String> {
        let fetched = self.load_jwks(issuer).await;
        let now = Instant::now();
        let mut guard = self.cache.write().await;
        match fetched {
            Ok((set, userinfo)) => {
                guard.insert(
                    issuer.to_string(),
                    CachedJwks {
                        set: Some(set.clone()),
                        userinfo,
                        fetched: now,
                        refreshed_at: now,
                    },
                );
                Ok(set)
            }
            Err(err) => {
                if let Some(cached) = guard.get_mut(issuer) {
                    cached.refreshed_at = now;
                    if let Some(set) = &cached.set {
                        return Ok(set.clone());
                    }
                } else {
                    guard.insert(
                        issuer.to_string(),
                        CachedJwks {
                            set: None,
                            userinfo: None,
                            fetched: now,
                            refreshed_at: now,
                        },
                    );
                }
                Err(err)
            }
        }
    }

    async fn load_jwks(&self, issuer: &str) -> Result<(JwkSet, Option<String>), String> {
        let discovery = discovery_document(&self.http, issuer).await?;
        let set = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await
            .map_err(|err| err.to_string())?
            .error_for_status()
            .map_err(|err| err.to_string())?
            .json::<JwkSet>()
            .await
            .map_err(|err| err.to_string())?;
        Ok((set, discovery.userinfo_endpoint))
    }
}

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AccountInfo {
    #[serde(default)]
    preferred_username: Option<String>,
}

fn account_local_name(value: &str) -> String {
    value
        .split_once('@')
        .map(|(name, _)| name)
        .unwrap_or(value)
        .to_string()
}

async fn discovery_document(http: &reqwest::Client, issuer: &str) -> Result<Discovery, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let doc = http
        .get(&url)
        .send()
        .await
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json::<Discovery>()
        .await
        .map_err(|err| err.to_string())?;
    if doc.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
        return Err("issuer discovery document does not match the configured issuer".into());
    }
    if !same_origin(&doc.jwks_uri, issuer) {
        return Err("signing keys are not published on the issuer origin".into());
    }
    if let Some(userinfo) = &doc.userinfo_endpoint {
        if !same_origin(userinfo, issuer) {
            return Err("account name endpoint is not on the issuer origin".into());
        }
    }
    Ok(doc)
}

fn same_origin(url: &str, issuer: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Ok(issuer) = reqwest::Url::parse(issuer) else {
        return false;
    };
    url.scheme() == issuer.scheme()
        && url.host_str() == issuer.host_str()
        && url.port() == issuer.port()
}

fn extra_clients() -> Result<Vec<TrustedClient>, String> {
    let issuers = std::env::var("OOKCITE_MCP_OIDC_EXTRA_ISSUERS").unwrap_or_default();
    let audiences = std::env::var("OOKCITE_MCP_OIDC_EXTRA_AUDIENCES").unwrap_or_default();
    let issuers = split_csv(&issuers);
    let audiences = split_csv(&audiences);
    if issuers.is_empty() && audiences.is_empty() {
        return Ok(Vec::new());
    }
    if issuers.len() != audiences.len() {
        return Err(
            "OOKCITE_MCP_OIDC_EXTRA_ISSUERS and OOKCITE_MCP_OIDC_EXTRA_AUDIENCES must have the same length"
                .into(),
        );
    }
    let mut clients = Vec::with_capacity(issuers.len());
    for (issuer, audience) in issuers.into_iter().zip(audiences) {
        if audience.is_empty()
            || audience
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("OOKCITE_MCP_OIDC_EXTRA_AUDIENCES must be single tokens".into());
        }
        clients.push(TrustedClient {
            issuer: require_https_url("OOKCITE_MCP_OIDC_EXTRA_ISSUERS", &issuer)?,
            audience,
        });
    }
    Ok(clients)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn push_unique(servers: &mut Vec<String>, issuer: &str) {
    let issuer = issuer.trim_end_matches('/').to_string();
    if !servers.iter().any(|item| item == &issuer) {
        servers.push(issuer);
    }
}

fn unverified_issuer(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let payload = parts.nth(1)?;
    if parts.next().is_none() || parts.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("iss")?.as_str().map(str::to_string)
}

fn required_token(name: &str) -> Result<String, String> {
    let value = std::env::var(name).map_err(|_| format!("{name} is required for OAuth"))?;
    let value = value.trim();
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{name} must be a single token"));
    }
    Ok(value.to_string())
}

fn required_https_url(name: &str) -> Result<String, String> {
    let value = std::env::var(name).map_err(|_| format!("{name} is required for OAuth"))?;
    require_https_url(name, value.trim())
}

fn require_https_url(name: &str, value: &str) -> Result<String, String> {
    let Ok(url) = reqwest::Url::parse(value) else {
        return Err(format!("{name} must be an absolute URL"));
    };
    let loopback = url
        .host_str()
        .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(format!("{name} must use https"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{name} contains a control character"));
    }
    Ok(value.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use jsonwebtoken::{EncodingKey, Header, encode};
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::traits::PublicKeyParts;

    fn policy() -> OidcPolicy {
        OidcPolicy {
            issuer: "https://id.example".into(),
            audience: "https://api.example".into(),
            extra: Vec::new(),
            scope: "openid".into(),
            resource: "https://api.example".into(),
        }
    }

    struct Signer {
        jwks: JwkSet,
        pem: String,
    }

    fn signer() -> Signer {
        let mut rng = rand::thread_rng();
        let private = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let n = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(private.n().to_bytes_be());
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(private.e().to_bytes_be());
        let jwks: JwkSet = serde_json::from_value(serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "kid": "test",
                "use": "sig",
                "alg": "RS256",
                "n": n,
                "e": e
            }]
        }))
        .unwrap();
        Signer { jwks, pem }
    }

    fn sign(signer: &Signer, iss: &str, aud: &str, scope: &str) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test".into());
        let body = serde_json::json!({
            "iss": iss,
            "aud": aud,
            "sub": "user-1",
            "preferred_username": "ada",
            "scope": scope,
            "iat": jsonwebtoken::get_current_timestamp(),
            "exp": jsonwebtoken::get_current_timestamp() + 60,
        });
        encode(
            &header,
            &body,
            &EncodingKey::from_rsa_pem(signer.pem.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn signed_token_with_audience_and_scope_is_accepted() {
        let signer = signer();
        let token = sign(
            &signer,
            "https://id.example",
            "https://api.example",
            "openid profile",
        );
        let subject = verify_access_token(&token, &policy(), &signer.jwks).unwrap();
        assert_eq!(subject.username, "ada");
        assert_eq!(subject.subject, "user-1");
    }

    #[test]
    fn wrong_audience_api_key_and_missing_scope_are_rejected() {
        let signer = signer();
        let bad_aud = sign(
            &signer,
            "https://id.example",
            "https://other.example",
            "openid",
        );
        assert!(verify_access_token(&bad_aud, &policy(), &signer.jwks).is_err());
        let no_scope = sign(
            &signer,
            "https://id.example",
            "https://api.example",
            "profile",
        );
        assert!(verify_access_token(&no_scope, &policy(), &signer.jwks).is_err());
        assert!(verify_access_token("ookc_live_key", &policy(), &signer.jwks).is_err());
    }

    #[test]
    fn a_second_client_is_advertised_and_accepted_beside_the_first() {
        let signer = signer();
        let mut policy = policy();
        policy.extra = vec![TrustedClient {
            issuer: "https://id.extra".into(),
            audience: "second-client".into(),
        }];
        let primary = sign(
            &signer,
            "https://id.example",
            "https://api.example",
            "openid",
        );
        let extra = sign(&signer, "https://id.extra", "second-client", "openid");
        let crossed = sign(&signer, "https://id.example", "second-client", "openid");
        let foreign = sign(&signer, "https://id.other", "second-client", "openid");
        assert_eq!(
            policy.metadata()["authorization_servers"],
            serde_json::json!(["https://id.extra", "https://id.example"])
        );
        assert!(verify_access_token(&primary, &policy, &signer.jwks).is_ok());
        assert!(verify_access_token(&extra, &policy, &signer.jwks).is_ok());
        assert!(verify_access_token(&crossed, &policy, &signer.jwks).is_err());
        assert!(verify_access_token(&foreign, &policy, &signer.jwks).is_err());
    }

    #[test]
    fn metadata_names_the_resource_and_issuer_only() {
        let body = policy().metadata();
        assert_eq!(body["resource"], "https://api.example");
        assert_eq!(body["authorization_servers"][0], "https://id.example");
        assert_eq!(body["scopes_supported"][0], "openid");
        assert_eq!(
            policy().metadata_url(),
            "https://api.example/.well-known/oauth-protected-resource"
        );
        assert!(policy().challenge().contains(&policy().metadata_url()));
    }

    #[test]
    fn metadata_url_puts_the_well_known_segment_between_host_and_path() {
        let mut policy = policy();
        policy.resource = "https://api.example/mcp".into();
        assert_eq!(policy.metadata()["resource"], "https://api.example/mcp");
        assert_eq!(
            policy.metadata_url(),
            "https://api.example/.well-known/oauth-protected-resource/mcp"
        );
        assert!(policy.challenge().contains(
            "resource_metadata=\"https://api.example/.well-known/oauth-protected-resource/mcp\""
        ));
    }

    #[test]
    fn from_env_reads_a_second_client() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("OOKCITE_MCP_OIDC_ISSUER", "https://id.example");
            std::env::set_var("OOKCITE_MCP_OIDC_AUDIENCE", "api");
            std::env::set_var("OOKCITE_MCP_RESOURCE", "https://api.example");
            std::env::set_var(
                "OOKCITE_MCP_OIDC_EXTRA_ISSUERS",
                "https://id.extra/oauth2/openid/second-client",
            );
            std::env::set_var("OOKCITE_MCP_OIDC_EXTRA_AUDIENCES", "second-client");
        }
        let policy = OidcPolicy::from_env().unwrap();
        unsafe {
            std::env::remove_var("OOKCITE_MCP_OIDC_ISSUER");
            std::env::remove_var("OOKCITE_MCP_OIDC_AUDIENCE");
            std::env::remove_var("OOKCITE_MCP_RESOURCE");
            std::env::remove_var("OOKCITE_MCP_OIDC_EXTRA_ISSUERS");
            std::env::remove_var("OOKCITE_MCP_OIDC_EXTRA_AUDIENCES");
        }
        assert_eq!(policy.issuer, "https://id.example");
        assert_eq!(policy.extra.len(), 1);
        assert_eq!(
            policy.extra[0].issuer,
            "https://id.extra/oauth2/openid/second-client"
        );
        assert_eq!(policy.extra[0].audience, "second-client");
        assert!(OidcPolicy::from_env().is_err());
    }
}


