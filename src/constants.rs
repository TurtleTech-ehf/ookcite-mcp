//! Shared constants and version helpers.

pub const API: &str = "https://ookcite-api.turtletech.us";

/// Effective API root (optional `OOKCITE_API` override, no trailing slash preference).
pub fn api_base_url() -> String {
    std::env::var("OOKCITE_API")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| API.to_string())
}
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MIN_CONFIDENT_REVERSE_LOOKUP_SCORE: f64 = 25.0;

/// Concurrent fan-out for read-only multi-lookup batches (verify_references, batch_format, …).
/// Override with OOKCITE_MCP_READ_CONCURRENCY (1–64).
pub const READ_ONLY_BATCH_CONCURRENCY: usize = 18;

/// Conservative concurrency for mutating multi-item tools (batch_add_to_collection).
pub const MUTATE_BATCH_CONCURRENCY: usize = 10;

/// Process-local exact-DOI metadata TTL (seconds). Identity-gated on store/load.
pub const DOI_CACHE_TTL_SECS: u64 = 600;

/// Anonymous daily cap. The sold catalogue owns the number; this constant
/// is the public crate's copy, and the catalogue crate checks it.
pub const ANON_DAILY_LOOKUPS: u32 = 40;

/// Free-account daily cap. Same source rule as [`ANON_DAILY_LOOKUPS`].
pub const FREE_DAILY_LOOKUPS: u32 = 60;

/// Anonymous / no-key: refuse metered batches larger than this.
/// The daily cap is [`ANON_DAILY_LOOKUPS`].
/// An audit also uses this cap when `/me` succeeds but omits
/// `lookups_remaining`. A failed `/me` refuses the audit instead.
pub const ANON_BATCH_SOFT_CAP: u32 = 40;

/// Largest provider response body the audit will read. A larger body is
/// refused while it is still streaming, not after it has been buffered.
pub const PROVIDER_BODY_LIMIT: usize = 1_048_576;

/// Largest manuscript `.bib` `audit_bibliography` will parse, in bytes.
pub const AUDIT_BIB_MAX_BYTES: usize = 1_048_576;

/// Largest number of BibTeX entries in one audit.
pub const AUDIT_BIB_MAX_ENTRIES: usize = 2_000;

/// Largest single BibTeX field value, in bytes, after concatenation.
pub const AUDIT_BIB_MAX_FIELD_BYTES: usize = 8_192;

/// Largest HTTP request body the streamable MCP endpoint accepts.
/// One bibliography at [`AUDIT_BIB_MAX_BYTES`] still fits inside the JSON
/// envelope. A larger body is refused with HTTP 413.
pub const HTTP_BODY_LIMIT: usize = 2_097_152;

/// Inputs the synchronous `/api/v1/resolve/batch` path accepts. The async mode
/// takes 1000 but answers with a job id, and no tool here polls jobs.
pub const SYNC_BATCH_RESOLVE_LIMIT: usize = 50;

/// When set to `1`/`true`/`yes`, MCP stdio startup runs blocking auth + npm
/// update probes on stderr before accepting connections. Default is off so
/// clients reach the server faster; probes are diagnostics only.
pub fn startup_probes_enabled() -> bool {
    match std::env::var("OOKCITE_STARTUP_PROBES") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        }
        Err(_) => false,
    }
}

pub fn version_output() -> String {
    format!("ookcite-mcp {VERSION}")
}

/// Compact nudge appended to 429 (rate-limited) tool responses. Kept to one
/// line, unlike `setup_help_block`, which documents every env var and is
/// reserved for auth-failure / blocked-mutation diagnostics.
pub fn rate_limit_hint() -> String {
    if crate::inbound_auth::has_api_key() {
        "Check remaining quota with the usage tool, or raise it: https://my.turtletech.us/signup?service=ookcite&source=ookcite_mcp"
            .to_string()
    } else {
        format!(
            "Anonymous cap is {ANON_DAILY_LOOKUPS} lookups/day. A free account raises that to {FREE_DAILY_LOOKUPS}/day: \
             sign up at https://my.turtletech.us/signup?service=ookcite&source=ookcite_mcp, then set OOKCITE_API_KEY \
             (or run `ookcite-mcp setup --key YOUR_KEY`)."
        )
    }
}

/// Soft warning before the hard 429. `remaining * 5 <= limit` is 80% used.
pub fn quota_soft_hint(remaining: u32, limit: u32) -> Option<String> {
    if limit == 0 || remaining.saturating_mul(5) > limit {
        return None;
    }
    if crate::inbound_auth::has_api_key() {
        Some(format!(
            "Near today's lookup cap ({remaining}/{limit}). Check the usage tool before a large batch."
        ))
    } else {
        Some(format!(
            "Near the anonymous cap ({remaining}/{limit}). A free account is {FREE_DAILY_LOOKUPS}/day plus collections: \
             https://my.turtletech.us/signup?service=ookcite&source=ookcite_mcp"
        ))
    }
}

/// Install and authentication guidance appended to client-facing errors.
pub fn setup_help_block() -> String {
    format!(
        "--- setup help ---\nookcite-mcp {VERSION}\nAPI (default): {API}\nOptional override: OOKCITE_API=https://host\nAuth: set OOKCITE_API_KEY (Bearer ookc_…) for higher limits + collections\n  Mint key: https://ookcite.turtletech.us (dashboard) or ttech-dashboard\nInstall / reconfigure:\n  ookcite-mcp setup --key YOUR_KEY\n  npx -y @turtletech/ookcite-mcp  (or cargo install --path .)\nClient policy:\n  OOKCITE_MCP_READ_ONLY=1     — hard-disable collection mutations\n  OOKCITE_MCP_ALLOW_MUTATE=0  — deny mutations (default allow if unset)\n  OOKCITE_STARTUP_PROBES=1    — stderr auth/update checks at MCP launch\nDiagnostics: use doctor or health_check tools; never paste full API keys into chat."
    )
}

/// Shared reqwest settings for OokCite API traffic: bounded timeouts, connection reuse.
pub fn build_api_client(
    timeout_secs: u64,
    default_headers: reqwest::header::HeaderMap,
) -> reqwest::Client {
    client_builder(timeout_secs)
        .default_headers(default_headers)
        .build()
        .expect("ookcite-mcp: failed to build HTTP client")
}

/// Client for doi.org, Crossref, DataCite, and Zenodo.
///
/// A redirect is followed only for `https` on port 443, with no userinfo,
/// and only when the host is one of `allowed_hosts`. The client also refuses
/// to start on `http`. Response bodies are capped separately by
/// [`PROVIDER_BODY_LIMIT`] at the read.
pub fn build_provider_client(
    timeout_secs: u64,
    allowed_hosts: impl IntoIterator<Item = String>,
) -> reqwest::Client {
    let allowed = allowed_host_set(allowed_hosts);
    client_builder(timeout_secs)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let allow = provider_redirect_target_allowed(attempt.url(), &allowed);
            redirect_action(attempt, allow)
        }))
        .build()
        .expect("ookcite-mcp: failed to build provider HTTP client")
}

/// `true` when a redirect target is https on port 443, has no userinfo, and
/// its host is one of the configured provider hosts.
pub(crate) fn provider_redirect_target_allowed(
    url: &reqwest::Url,
    allowed_hosts: &std::collections::HashSet<String>,
) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && allowed_hosts.contains(&url.host_str().unwrap_or("").to_ascii_lowercase())
}

fn allowed_host_set(
    hosts: impl IntoIterator<Item = String>,
) -> std::sync::Arc<std::collections::HashSet<String>> {
    std::sync::Arc::new(
        hosts
            .into_iter()
            .map(|host| host.to_ascii_lowercase())
            .collect(),
    )
}

fn redirect_action(
    attempt: reqwest::redirect::Attempt<'_>,
    allow: bool,
) -> reqwest::redirect::Action {
    if attempt.previous().len() < 4 && allow {
        attempt.follow()
    } else {
        attempt.error("provider redirect refused")
    }
}

/// HTTP client for a test mock. Production [`build_provider_client`] is
/// https-only on port 443 and does not call this.
///
/// The first request may use the mock's own scheme and port. A redirect is
/// followed only back to that exact origin, or when it passes
/// [`provider_redirect_target_allowed`] for `allowed_hosts`.
#[cfg(test)]
pub(crate) fn build_provider_client_for_tests(
    timeout_secs: u64,
    allowed_hosts: impl IntoIterator<Item = String>,
    mock_origins: impl IntoIterator<Item = TestProviderOrigin>,
) -> reqwest::Client {
    let allowed = allowed_host_set(allowed_hosts);
    let origins: std::sync::Arc<Vec<TestProviderOrigin>> =
        std::sync::Arc::new(mock_origins.into_iter().collect());
    client_builder(timeout_secs)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let allow = {
                let url = attempt.url();
                origins.iter().any(|origin| origin.matches(url))
                    || provider_redirect_target_allowed(url, &allowed)
            };
            redirect_action(attempt, allow)
        }))
        .build()
        .expect("ookcite-mcp: failed to build test provider HTTP client")
}

/// One mock base URL. Scheme, host, and port must all match.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct TestProviderOrigin {
    scheme: String,
    host: String,
    port: u16,
}

#[cfg(test)]
impl TestProviderOrigin {
    pub(crate) fn parse(base: &str) -> Self {
        let url = reqwest::Url::parse(base).expect("test provider origin");
        Self {
            scheme: url.scheme().to_string(),
            host: url
                .host_str()
                .expect("test provider host")
                .to_ascii_lowercase(),
            port: url.port_or_known_default().expect("test provider port"),
        }
    }

    fn matches(&self, url: &reqwest::Url) -> bool {
        url.scheme() == self.scheme
            && url
                .host_str()
                .unwrap_or("")
                .eq_ignore_ascii_case(&self.host)
            && url.port_or_known_default() == Some(self.port)
            && url.username().is_empty()
            && url.password().is_none()
    }
}

fn client_builder(timeout_secs: u64) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10.min(timeout_secs)))
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_keepalive(std::time::Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_catalogue_client_pin_is_40_60_40() {
        assert_eq!(
            (ANON_DAILY_LOOKUPS, FREE_DAILY_LOOKUPS, ANON_BATCH_SOFT_CAP,),
            (40, 60, 40),
            "anonymous daily, free daily, and anonymous batch soft cap"
        );
    }

    #[test]
    fn startup_probes_default_off() {
        // Unset in normal test runs unless the harness injects the var.
        if std::env::var("OOKCITE_STARTUP_PROBES").is_err() {
            assert!(!startup_probes_enabled());
        }
    }

    #[test]
    fn quota_soft_hint_fires_at_eighty_percent() {
        assert!(quota_soft_hint(4, 20).is_some());
        assert!(quota_soft_hint(5, 20).is_none());
        assert!(quota_soft_hint(0, 20).is_some());
        assert!(quota_soft_hint(1, 0).is_none());
    }

    #[test]
    fn build_api_client_constructs() {
        let client = build_api_client(5, reqwest::header::HeaderMap::new());
        // Client is cloneable/reusable; construction is the perf-critical path.
        let _ = client.clone();
    }

    #[test]
    fn socks5h_proxy_is_a_known_scheme() {
        // HTTPS_PROXY on this seat is socks5h. Without the socks feature,
        // reqwest rejects that scheme and every call dies as
        // "error sending request for url".
        reqwest::Proxy::all("socks5h://127.0.0.1:1080").expect("socks5h proxy");
    }

    #[test]
    fn provider_redirect_requires_https_on_port_443_without_userinfo() {
        let hosts = std::collections::HashSet::from(["doi.org".to_string()]);
        let allowed = |raw: &str| {
            provider_redirect_target_allowed(&reqwest::Url::parse(raw).unwrap(), &hosts)
        };
        assert!(allowed("https://doi.org/api/handles/10.1000/x"));
        assert!(allowed("https://doi.org:443/api/handles/10.1000/x"));
        assert!(
            !allowed("https://doi.org:444/api/handles/10.1000/x"),
            "an allowed host on another port"
        );
        assert!(
            !allowed("http://doi.org:443/api/handles/10.1000/x"),
            "https to http downgrade"
        );
        assert!(
            !allowed("http://doi.org/api/handles/10.1000/x"),
            "an http hop"
        );
        assert!(
            !allowed("https://user@doi.org/api/handles/10.1000/x"),
            "userinfo in the redirect URL"
        );
        assert!(!allowed(
            "https://user:secret@doi.org/api/handles/10.1000/x"
        ));
        assert!(!allowed("https://evil.example/api/handles/10.1000/x"));
    }

    #[tokio::test]
    async fn provider_client_refuses_an_http_start() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("body"))
            .expect(0)
            .mount(&mock)
            .await;
        let client = build_provider_client(5, ["127.0.0.1".to_string()]);
        let error = client
            .get(format!("{}/handles/10.1000/x", mock.uri()))
            .send()
            .await
            .expect_err("http start must fail before the mock is called");
        assert!(error.is_builder(), "{error:?}");
        assert!(
            error_chain(&error).contains("URL scheme is not allowed"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn provider_redirect_refuses_another_port_a_downgrade_and_userinfo() {
        assert_redirect_refused("https://doi.org:444/api/handles/10.1000/x").await;
        assert_redirect_refused("https://127.0.0.1:444/handles/10.1000/x").await;
        assert_redirect_refused("http://doi.org:443/api/handles/10.1000/x").await;
        assert_redirect_refused("http://127.0.0.1:443/handles/10.1000/x").await;
        assert_redirect_refused("https://user@doi.org/api/handles/10.1000/x").await;
        assert_redirect_refused("https://user@127.0.0.1/handles/10.1000/x").await;
    }

    async fn assert_redirect_refused(location: &str) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(302).insert_header("location", location))
            .expect(1)
            .mount(&mock)
            .await;
        let client = build_provider_client_for_tests(
            5,
            ["doi.org".to_string(), "127.0.0.1".to_string()],
            [TestProviderOrigin::parse(&mock.uri())],
        );
        let error = client
            .get(format!("{}/handles/10.1000/x", mock.uri()))
            .send()
            .await
            .expect_err("redirect must be refused");
        assert!(error.is_redirect(), "{error:?}");
        assert!(
            error_chain(&error).contains("provider redirect refused"),
            "{error:?}"
        );
    }

    fn error_chain(error: &reqwest::Error) -> String {
        use std::error::Error;
        let mut parts = vec![error.to_string()];
        let mut source = error.source();
        while let Some(err) = source {
            parts.push(err.to_string());
            source = err.source();
        }
        parts.join(" ")
    }
}
