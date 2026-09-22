//! OAuth resource server for the remote MCP endpoint.
//!
//! Callers sign in and send a short-lived access token. This module checks
//! that token against the configured issuer's published keys. It does not
//! accept an API key, and it does not embed an issuer address.

use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

const JWKS_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcPolicy {
    pub issuer: String,
    pub audience: String,
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
            scope,
            resource,
        })
    }

    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "resource": self.resource,
            "authorization_servers": [self.issuer],
            "bearer_methods_supported": ["header"],
            "scopes_supported": [self.scope],
        })
    }

    pub fn metadata_url(&self) -> String {
        format!(
            "{}/.well-known/oauth-protected-resource",
            self.resource.trim_end_matches('/')
        )
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

pub(crate) fn verify_access_token(
    token: &str,
    policy: &OidcPolicy,
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
    validation.set_issuer(&[&policy.issuer]);
    validation.set_audience(&[&policy.audience]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    let data =
        decode::<AccessClaims>(token, &key, &validation).map_err(|_| VerifyFail::Rejected)?;
    if data.claims.exp.saturating_sub(data.claims.iat) > 3600 {
        return Err(VerifyFail::Rejected);
    }
    if !has_scope(data.claims.scope.as_deref(), &policy.scope) {
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
    cache: tokio::sync::RwLock<Option<CachedJwks>>,
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
            cache: tokio::sync::RwLock::new(None),
            refresh: tokio::sync::Mutex::new(()),
        })
    }

    pub fn policy(&self) -> &OidcPolicy {
        &self.policy
    }

    pub async fn verify(&self, token: &str) -> Result<AccessSubject, ()> {
        let jwks = self.jwks().await.map_err(|_| ())?;
        match verify_access_token(token, &self.policy, &jwks) {
            Ok(subject) => self.with_account_name(token, subject).await,
            Err(VerifyFail::Rejected) => Err(()),
            Err(VerifyFail::UnknownKey) => {
                let jwks = self.refresh_for_unknown_key().await.map_err(|_| ())?;
                match verify_access_token(token, &self.policy, &jwks) {
                    Ok(subject) => self.with_account_name(token, subject).await,
                    Err(_) => Err(()),
                }
            }
        }
    }

    async fn with_account_name(
        &self,
        token: &str,
        mut subject: AccessSubject,
    ) -> Result<AccessSubject, ()> {
        if subject.username != subject.subject {
            return Ok(subject);
        }
        let name = self.account_name(token).await.map_err(|_| ())?;
        subject.username = name;
        Ok(subject)
    }

    async fn account_name(&self, token: &str) -> Result<String, String> {
        let url = self
            .cache
            .read()
            .await
            .as_ref()
            .and_then(|cached| cached.userinfo.clone())
            .ok_or_else(|| "account name endpoint is not published".to_string())?;
        if !same_origin(&url, &self.policy.issuer) {
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

    async fn refresh_for_unknown_key(&self) -> Result<JwkSet, String> {
        let _hold = self.refresh.lock().await;
        if let Some(cached) = self.cache.read().await.as_ref() {
            if cached.refreshed_at.elapsed() < Duration::from_secs(30) {
                return cached
                    .set
                    .clone()
                    .ok_or_else(|| "signing keys unavailable".into());
            }
        }
        self.fetch_keys().await
    }

    async fn jwks(&self) -> Result<JwkSet, String> {
        let _hold = self.refresh.lock().await;
        if let Some(cached) = self.cache.read().await.as_ref() {
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
        self.fetch_keys().await
    }

    async fn fetch_keys(&self) -> Result<JwkSet, String> {
        let fetched = self.load_jwks().await;
        let now = Instant::now();
        let mut guard = self.cache.write().await;
        match fetched {
            Ok((set, userinfo)) => {
                *guard = Some(CachedJwks {
                    set: Some(set.clone()),
                    userinfo,
                    fetched: now,
                    refreshed_at: now,
                });
                Ok(set)
            }
            Err(err) => {
                if let Some(cached) = guard.as_mut() {
                    cached.refreshed_at = now;
                    if let Some(set) = &cached.set {
                        return Ok(set.clone());
                    }
                } else {
                    *guard = Some(CachedJwks {
                        set: None,
                        userinfo: None,
                        fetched: now,
                        refreshed_at: now,
                    });
                }
                Err(err)
            }
        }
    }

    async fn load_jwks(&self) -> Result<(JwkSet, Option<String>), String> {
        let discovery = discovery_document(&self.http, &self.policy.issuer).await?;
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
    value.split_once('@').map(|(name, _)| name).unwrap_or(value).to_string()
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
    use jsonwebtoken::{encode, EncodingKey, Header};
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::traits::PublicKeyParts;

    fn policy() -> OidcPolicy {
        OidcPolicy {
            issuer: "https://id.example".into(),
            audience: "https://api.example".into(),
            scope: "openid".into(),
            resource: "https://api.example".into(),
        }
    }

    fn jwks_and_token(aud: &str, scope: &str) -> (JwkSet, String) {
        let mut rng = rand::thread_rng();
        let private = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = private.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
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
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test".into());
        let body = serde_json::json!({
            "iss": "https://id.example",
            "aud": aud,
            "sub": "user-1",
            "preferred_username": "ada",
            "scope": scope,
            "iat": jsonwebtoken::get_current_timestamp(),
            "exp": jsonwebtoken::get_current_timestamp() + 60,
        });
        let token = encode(
            &header,
            &body,
            &EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        )
        .unwrap();
        (jwks, token)
    }

    #[test]
    fn signed_token_with_audience_and_scope_is_accepted() {
        let (jwks, token) = jwks_and_token("https://api.example", "openid profile");
        let subject = verify_access_token(&token, &policy(), &jwks).unwrap();
        assert_eq!(subject.username, "ada");
        assert_eq!(subject.subject, "user-1");
    }

    #[test]
    fn wrong_audience_api_key_and_missing_scope_are_rejected() {
        let (jwks, bad_aud) = jwks_and_token("https://other.example", "openid");
        assert!(verify_access_token(&bad_aud, &policy(), &jwks).is_err());
        let (jwks, no_scope) = jwks_and_token("https://api.example", "profile");
        assert!(verify_access_token(&no_scope, &policy(), &jwks).is_err());
        assert!(verify_access_token("ookc_live_key", &policy(), &jwks).is_err());
    }

    #[test]
    fn metadata_names_the_resource_and_issuer_only() {
        let body = policy().metadata();
        assert_eq!(body["resource"], "https://api.example");
        assert_eq!(body["authorization_servers"][0], "https://id.example");
        assert_eq!(body["scopes_supported"][0], "openid");
        assert!(policy().challenge().contains("resource_metadata="));
    }
}
