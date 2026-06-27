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

/// Install / auth guidance appended to agent-facing errors (nimvault doctor style).
pub fn setup_help_block() -> String {
    format!(
        "--- setup help ---\nookcite-mcp {VERSION}\nAPI (default): {API}\nOptional override: OOKCITE_API=https://host\nAuth: set OOKCITE_API_KEY (Bearer ookc_…) for higher limits + collections\n  Mint key: https://ookcite.turtletech.us (dashboard) or ttech-dashboard\nInstall / reconfigure:\n  ookcite-mcp setup --key YOUR_KEY\n  npx -y @turtletech/ookcite-mcp  (or cargo install --path .)\nAgent policy:\n  OOKCITE_MCP_READ_ONLY=1     — hard-disable collection mutations\n  OOKCITE_MCP_ALLOW_MUTATE=0  — deny mutations (default allow if unset)\n  OOKCITE_STARTUP_PROBES=1    — stderr auth/update checks at MCP launch\nDiagnostics: use doctor or health_check tools; never paste full API keys into chat."
    )
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
