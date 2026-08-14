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

/// Anonymous / no-key: refuse metered batches larger than this (IP daily limit ~20).
pub const ANON_BATCH_SOFT_CAP: u32 = 8;

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
    if std::env::var("OOKCITE_API_KEY").is_ok() {
        "Check remaining quota with the usage tool, or raise it: https://my.turtletech.us/signup"
            .to_string()
    } else {
        "Anonymous cap is 20 lookups/day. A free account raises that to 60/day: \
         sign up at https://my.turtletech.us/signup, then set OOKCITE_API_KEY \
         (or run `ookcite-mcp setup --key YOUR_KEY`)."
            .to_string()
    }
}

/// Install and authentication guidance appended to client-facing errors.
pub fn setup_help_block() -> String {
    format!(
        "--- setup help ---\nookcite-mcp {VERSION}\nAPI (default): {API}\nOptional override: OOKCITE_API=https://host\nAuth: set OOKCITE_API_KEY (Bearer ookc_…) for higher limits + collections\n  Mint key: https://ookcite.turtletech.us (dashboard) or ttech-dashboard\nInstall / reconfigure:\n  ookcite-mcp setup --key YOUR_KEY\n  npx -y @turtletech/ookcite-mcp  (or cargo install --path .)\nClient policy:\n  OOKCITE_MCP_READ_ONLY=1     — hard-disable collection mutations\n  OOKCITE_MCP_ALLOW_MUTATE=0  — deny mutations (default allow if unset)\n  OOKCITE_STARTUP_PROBES=1    — stderr auth/update checks at MCP launch\nDiagnostics: use doctor or health_check tools; never paste full API keys into chat."
    )
}

/// Shared reqwest settings for API traffic: bounded timeouts, connection reuse.
pub fn build_api_client(
    timeout_secs: u64,
    default_headers: reqwest::header::HeaderMap,
) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10.min(timeout_secs)))
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .default_headers(default_headers)
        .build()
        .expect("ookcite-mcp: failed to build HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_probes_default_off() {
        // Unset in normal test runs unless the harness injects the var.
        if std::env::var("OOKCITE_STARTUP_PROBES").is_err() {
            assert!(!startup_probes_enabled());
        }
    }

    #[test]
    fn build_api_client_constructs() {
        let client = build_api_client(5, reqwest::header::HeaderMap::new());
        // Client is cloneable/reusable; construction is the perf-critical path.
        let _ = client.clone();
    }
}
