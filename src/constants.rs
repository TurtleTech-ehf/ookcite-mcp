//! Shared constants and version helpers.

pub const API: &str = "https://ookcite-api.turtletech.us";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MIN_CONFIDENT_REVERSE_LOOKUP_SCORE: f64 = 25.0;

pub fn version_output() -> String {
    format!("ookcite-mcp {VERSION}")
}
