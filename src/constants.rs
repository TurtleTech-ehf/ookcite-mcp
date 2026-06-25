//! Shared constants and version helpers.

pub const API: &str = "https://ookcite-api.turtletech.us";
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
