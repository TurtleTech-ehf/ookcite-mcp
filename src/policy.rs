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
    mutate_explicitly_allowed().unwrap_or(true)
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
            lines.push(format!("OOKCITE_API override: {}", redact_url(base.trim())));
        }
    }
    lines
}

/// Drop userinfo, query, and fragment from one URL. The path stays so a
/// diagnostic can still name the route.
pub fn redact_url(raw: &str) -> String {
    let without_fragment = raw.split_once('#').map(|(head, _)| head).unwrap_or(raw);
    let without_query = without_fragment
        .split_once('?')
        .map(|(head, _)| head)
        .unwrap_or(without_fragment);
    let Some(scheme_end) = without_query.find("://") else {
        return without_query.to_string();
    };
    let rest = &without_query[scheme_end + 3..];
    let slash = rest.find('/').unwrap_or(rest.len());
    if let Some(at) = rest[..slash].find('@') {
        return format!("{}{}", &without_query[..scheme_end + 3], &rest[at + 1..]);
    }
    without_query.to_string()
}

/// Replace every http(s) URL in `text` with [`redact_url`].
pub fn redact_urls_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = find_url_start(rest) {
        out.push_str(&rest[..start]);
        let url_text = &rest[start..];
        let end = url_text
            .find(|ch: char| ch.is_whitespace() || matches!(ch, ')' | '>' | '"' | '\'' | ','))
            .unwrap_or(url_text.len());
        // A delimiter at the start of this slice is not a URL. Advancing
        // one character keeps a zero-width match from spinning.
        if end == 0 {
            let mut chars = url_text.chars();
            let Some(ch) = chars.next() else {
                break;
            };
            out.push(ch);
            rest = chars.as_str();
            continue;
        }
        out.push_str(&redact_url(&url_text[..end]));
        rest = &url_text[end..];
    }
    out.push_str(rest);
    out
}

fn find_url_start(text: &str) -> Option<usize> {
    let https = text.find("https://");
    let http = text.find("http://");
    match (https, http) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// A reqwest `Display` includes the full URL. Diagnostics print this instead.
pub fn scrub_error(error: &impl std::fmt::Display) -> String {
    redact_urls_in_text(&error.to_string())
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
    fn a_diagnostic_url_drops_userinfo_and_query() {
        assert_eq!(
            redact_url("http://alice:hunter2@127.0.0.1:9/api/v1/lookup/doi?api_key=secret"),
            "http://127.0.0.1:9/api/v1/lookup/doi"
        );
        assert_eq!(
            redact_url("https://127.0.0.1/works?token=abc#frag"),
            "https://127.0.0.1/works"
        );
        let text = redact_urls_in_text(
            "error sending request for url (http://alice:hunter2@127.0.0.1:9/api?api_key=secret_query)",
        );
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("alice"), "{text}");
        assert!(!text.contains("secret_query"), "{text}");
        assert!(text.contains("http://127.0.0.1:9/api"), "{text}");
        assert!(text.ends_with(')'), "{text}");
        let scrubbed = scrub_error(
            &"error sending request for url (http://alice:hunter2@127.0.0.1:9/api?api_key=secret_query)",
        );
        assert!(scrubbed.contains("error sending request"), "{scrubbed}");
        assert!(scrubbed.contains("http://127.0.0.1:9/api"), "{scrubbed}");
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
        assert!(!scrubbed.contains("secret_query"), "{scrubbed}");
    }

    #[test]
    fn mutate_block_mentions_setup() {
        let msg = mutate_block_message();
        assert!(msg.contains("BLOCKED"));
        assert!(msg.contains("ookcite-mcp setup") || msg.contains("OOKCITE_API_KEY"));
    }
}
