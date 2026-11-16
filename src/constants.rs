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
/// An audit also uses this cap when a key is present but `/me` did not
/// confirm `lookups_remaining`.
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
/// A redirect is followed only when the next host is one of `allowed_hosts`
/// (the hosts those providers were configured with) and only for a few hops.
/// Any other host stops the request. Response bodies are capped separately
/// by [`PROVIDER_BODY_LIMIT`] at the read.
pub fn build_provider_client(
    timeout_secs: u64,
    allowed_hosts: impl IntoIterator<Item = String>,
) -> reqwest::Client {
    let allowed: std::sync::Arc<std::collections::HashSet<String>> = std::sync::Arc::new(
        allowed_hosts
            .into_iter()
            .map(|host| host.to_ascii_lowercase())
            .collect(),
    );
    client_builder(timeout_secs)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let host = attempt.url().host_str().unwrap_or("").to_ascii_lowercase();
            if attempt.previous().len() < 4 && allowed.contains(&host) {
                attempt.follow()
            } else {
                attempt.error("provider redirect refused")
            }
        }))
        .build()
        .expect("ookcite-mcp: failed to build provider HTTP client")
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
}
