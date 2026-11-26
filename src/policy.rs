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
/// diagnostic can still name the route. Every authority is cleaned, including
/// a nested `http://user:pass@host` in the path and a percent-encoded
/// `http%3A%2F%2Fuser:pass%40host`.
pub fn redact_url(raw: &str) -> String {
    let without_fragment = raw.split_once('#').map(|(head, _)| head).unwrap_or(raw);
    let without_query = without_fragment
        .split_once('?')
        .map(|(head, _)| head)
        .unwrap_or(without_fragment);
    strip_encoded_userinfo(&strip_userinfo(without_query))
}

/// `scheme_end + "://".len()` is one offset shared by the prefix and the
/// authority. Two copies of `+ 3` let a mutant flip only the authority
/// index and still emit a host that the older equality tests accept.
fn strip_userinfo(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(scheme_end) = rest.find("://") {
        let authority_at = scheme_end + "://".len();
        out.push_str(&rest[..authority_at]);
        let after = &rest[authority_at..];
        let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let authority = &after[..auth_end];
        match authority.rsplit_once('@') {
            Some((_, host)) => out.push_str(host),
            None => out.push_str(authority),
        }
        rest = &after[auth_end..];
    }
    out.push_str(rest);
    out
}

fn strip_encoded_userinfo(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(url.len());
    let mut index = 0;
    while index < url.len() {
        if let Some(marker_len) = encoded_http_scheme_at(bytes, index) {
            out.push_str(&url[index..index + marker_len]);
            let after = index + marker_len;
            if let Some(at_rel) = lower[after..].find("%40") {
                let userinfo = &lower[after..after + at_rel];
                if !userinfo.contains(['/', '?', '#']) {
                    index = after + at_rel + "%40".len();
                    continue;
                }
            }
            index = after;
            continue;
        }
        let ch = url[index..].chars().next().unwrap();
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn encoded_http_scheme_at(bytes: &[u8], index: usize) -> Option<usize> {
    const HTTPS: &[u8] = b"https%3a%2f%2f";
    const HTTP: &[u8] = b"http%3a%2f%2f";
    if bytes[index..].starts_with(HTTPS) {
        Some(HTTPS.len())
    } else if bytes[index..].starts_with(HTTP) {
        Some(HTTP.len())
    } else {
        None
    }
}

/// Replace every http(s) URL in `text` with [`redact_url`].
///
/// A comma or a parenthesis inside the URL is part of the URL. `Foo_(bar)`
/// and `?filter=a,b&api_key=` used to end the token early, so the query tail
/// never reached [`redact_url`]. A `)` ends the URL only when it closes a
/// wrapper that started before the URL, which is how reqwest prints
/// `error sending request for url (http://...)`.
pub fn redact_urls_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = find_url_start(rest) {
        out.push_str(&rest[..start]);
        let url_text = &rest[start..];
        let end = http_url_end(url_text);
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

fn http_url_end(url_text: &str) -> usize {
    let mut depth = 0usize;
    for (index, ch) in url_text.char_indices() {
        match ch {
            '(' => depth = depth.saturating_add(1),
            ')' if depth == 0 => return index,
            ')' => depth -= 1,
            ch if ch.is_whitespace() || matches!(ch, '>' | '"' | '\'') => return index,
            _ => {}
        }
    }
    url_text.len()
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
    fn diagnostic_urls_drop_userinfo_query_and_fragment_for_ipv6_percent_and_nested() {
        let ipv6 = redact_url("http://alice:hunter2@[::1]:9/api?api_key=secret#frag");
        assert_eq!(ipv6, "http://[::1]:9/api", "{ipv6}");
        assert!(!ipv6.contains("hunter2"), "{ipv6}");
        assert!(!ipv6.contains("secret"), "{ipv6}");
        assert!(!ipv6.contains("frag"), "{ipv6}");

        let encoded = redact_urls_in_text(
            "error sending request for url (http://alice:hunter%3A2@[2001:db8::1]/v1?token=secret#x)",
        );
        assert!(!encoded.contains("hunter"), "{encoded}");
        assert!(!encoded.contains("alice"), "{encoded}");
        assert!(!encoded.contains("secret"), "{encoded}");
        assert!(!encoded.contains('#'), "{encoded}");

        let nested = redact_urls_in_text(
            "error sending request for url (http://127.0.0.1/hop/http://alice:hunter2@evil.test/secret?api_key=secret)",
        );
        assert!(!nested.contains("hunter2"), "{nested}");
        assert!(!nested.contains("alice:"), "{nested}");
        assert!(!nested.contains("api_key"), "{nested}");

        let nested_pct = redact_urls_in_text(
            "error sending request for url (http://127.0.0.1/hop/http%3A%2F%2Falice:hunter2%40evil.test/x?token=secret)",
        );
        assert!(!nested_pct.contains("hunter2"), "{nested_pct}");
        assert!(!nested_pct.contains("secret"), "{nested_pct}");

        let comma = redact_urls_in_text(
            "error sending request for url (http://alice:hunter2@127.0.0.1:9/api?filter=a,b&api_key=secret)",
        );
        assert!(!comma.contains("hunter2"), "{comma}");
        assert!(!comma.contains("api_key"), "{comma}");
        assert!(!comma.contains("secret"), "{comma}");

        let paren = redact_urls_in_text(
            "error sending request for url (http://alice:hunter2@127.0.0.1/wiki/Foo_(bar)?api_key=secret)",
        );
        assert!(!paren.contains("hunter2"), "{paren}");
        assert!(!paren.contains("api_key"), "{paren}");
        assert!(!paren.contains("secret"), "{paren}");
    }

    #[test]
    fn redact_url_uses_one_scheme_offset_and_ignores_non_url_text() {
        // `scheme_end * 3` keeps a slice of the userinfo glued to the host.
        assert_eq!(
            redact_url("https://user:secret@example.com/a"),
            "https://example.com/a"
        );
        // `find_url_start` returning `Some(0)` would treat `note?keep#frag`
        // as the URL and drop the query and the fragment.
        assert_eq!(
            redact_urls_in_text("note?keep#frag before http://example.com/a"),
            "note?keep#frag before http://example.com/a"
        );
    }

    #[test]
    fn mutate_block_mentions_setup() {
        let msg = mutate_block_message();
        assert!(msg.contains("BLOCKED"));
        assert!(msg.contains("ookcite-mcp setup") || msg.contains("OOKCITE_API_KEY"));
    }
}
