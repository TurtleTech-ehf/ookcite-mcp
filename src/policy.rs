//! Agent-session policy: read-only / mutate gates (nimvault-mcp pattern).
//!
//! Collection writes and other remote mutations are blocked unless explicitly
//! allowed. Default allows mutate when an API key is present so existing
//! setups keep working; set `OOKCITE_MCP_READ_ONLY=1` for review agents or
//! `OOKCITE_MCP_ALLOW_MUTATE=0` to deny writes even with a key.

use crate::constants::{setup_help_block, VERSION};

fn env_truthy(name: &str) -> Option<bool> {
    let v = std::env::var(name).ok()?;
    let v = v.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    Some(matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Hard lock: all mutating tools disabled regardless of API key.
pub fn read_only_locked() -> bool {
    env_truthy("OOKCITE_MCP_READ_ONLY") == Some(true)
}

/// When `OOKCITE_MCP_ALLOW_MUTATE` is set, it wins (true allows, false denies).
/// When unset, mutate is allowed (collections need a key at the API layer).
pub fn mutate_explicitly_allowed() -> Option<bool> {
    env_truthy("OOKCITE_MCP_ALLOW_MUTATE")
}

/// Whether collection / remote-write tools may run in this process.
pub fn mutate_allowed() -> bool {
    if read_only_locked() {
        return false;
    }
    match mutate_explicitly_allowed() {
        Some(v) => v,
        None => true,
    }
}

/// Message returned from mutating tools when blocked; `None` if allowed.
pub fn block_mutate() -> Option<String> {
    if mutate_allowed() {
        return None;
    }
    Some(mutate_block_message())
}

pub fn mutate_block_message() -> String {
    if read_only_locked() {
        return format!(
            "BLOCKED: OOKCITE_MCP_READ_ONLY=1 — collection and other mutating \
             tools are disabled (hard lock). Read-only tools (validate_doi, \
             format_citation, health_check, doctor, list/search/export \
             collections) remain available.\n\n{}",
            setup_help_block()
        );
    }
    format!(
        "BLOCKED: set OOKCITE_MCP_ALLOW_MUTATE=1 in the MCP server environment \
         to enable collection writes (add/remove/import/share/merge/…). \
         Default is allow unless ALLOW_MUTATE=0 or READ_ONLY=1.\n\n{}",
        setup_help_block()
    )
}

/// Never echo a full API key; agents only need presence + last-4.
pub fn redact_api_key_hint(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => "unset".into(),
        Some(k) if k.len() <= 4 => "set (…****)".into(),
        Some(k) => format!("set (…{})", &k[k.len() - 4..]),
    }
}

pub fn policy_summary_lines() -> Vec<String> {
    let mut lines = vec![
        format!("ookcite-mcp {VERSION}"),
        format!(
            "mutate tools: {}",
            if mutate_allowed() {
                "allowed"
            } else if read_only_locked() {
                "disabled (OOKCITE_MCP_READ_ONLY=1)"
            } else {
                "disabled (OOKCITE_MCP_ALLOW_MUTATE=0)"
            }
        ),
        format!(
            "OOKCITE_API_KEY: {}",
            redact_api_key_hint(std::env::var("OOKCITE_API_KEY").ok().as_deref())
        ),
    ];
    if let Ok(base) = std::env::var("OOKCITE_API") {
        if !base.trim().is_empty() {
            lines.push(format!("OOKCITE_API override: {}", base.trim()));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_hides_key_body() {
        assert_eq!(redact_api_key_hint(None), "unset");
        assert_eq!(
            redact_api_key_hint(Some("ookc_abcdefghijklmnop")),
            "set (…mnop)"
        );
    }

    #[test]
    fn mutate_block_mentions_setup() {
        let msg = mutate_block_message();
        assert!(msg.contains("BLOCKED"));
        assert!(msg.contains("ookcite-mcp setup") || msg.contains("OOKCITE_API_KEY"));
    }
}
