//! Collection entry identity helpers (canonical ids, DOI aliases, search lines).

pub fn normalize_doi_token(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_prefix = trimmed
        .strip_prefix("doi:")
        .or_else(|| trimmed.strip_prefix("DOI:"))
        .unwrap_or(trimmed)
        .trim();
    without_prefix.to_ascii_lowercase()
}

/// True when `raw` looks like a bare DOI or `doi:…` token (not a UUID/opaque id).
pub fn looks_like_doi_token(raw: &str) -> bool {
    let n = normalize_doi_token(raw);
    n.starts_with("10.") && n.contains('/')
}

/// Canonical entry id from a collection entry object (`id` field, else `doi:<doi>`).
pub fn entry_canonical_id(entry: &serde_json::Value) -> Option<String> {
    if let Some(id) = entry["id"].as_str().filter(|s| !s.is_empty()) {
        return Some(id.to_string());
    }
    let doi = entry_doi(entry)?;
    Some(format!("doi:{}", normalize_doi_token(&doi)))
}

/// DOI from entry metadata, if present.
pub fn entry_doi(entry: &serde_json::Value) -> Option<String> {
    entry["metadata"]["doi"]
        .as_str()
        .or_else(|| entry["doi"].as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Format one collection entry line for agent-facing search output.
pub fn format_collection_entry_line(entry: &serde_json::Value) -> String {
    let entry_id = entry_canonical_id(entry).unwrap_or_else(|| "?".into());
    let meta = &entry["metadata"];
    let title = meta["title"].as_str().unwrap_or("?");
    let authors = meta["authors"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p["family"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let year = meta["date"]["year"]
        .as_i64()
        .map(|y| format!(" ({y})"))
        .unwrap_or_default();
    let doi_hint = entry_doi(entry)
        .map(|d| format!("; aliases: doi:{}", normalize_doi_token(&d)))
        .unwrap_or_default();
    // Only add alias hint when the canonical id is not already the doi: form.
    let doi_hint = if entry_id.strip_prefix("doi:").is_some_and(|rest| {
        Some(rest.to_string()) == entry_doi(entry).map(|d| normalize_doi_token(&d))
    }) {
        String::new()
    } else {
        doi_hint
    };
    format!("- entry_id: {entry_id}{doi_hint}; {authors}{year}: {title}")
}

/// Resolve a user-supplied entry reference (opaque id, `doi:…`, or bare DOI) to the
/// canonical entry `id` used by the DELETE endpoint. Returns `None` if no match.
pub fn resolve_entry_id_in_collection(
    entries: &[serde_json::Value],
    needle: &str,
) -> Option<String> {
    let needle_trim = needle.trim();
    if needle_trim.is_empty() {
        return None;
    }

    // 1. Exact id match
    for e in entries {
        if let Some(id) = e["id"].as_str() {
            if id == needle_trim {
                return Some(id.to_string());
            }
        }
    }

    // 2. Case-insensitive exact id match
    let needle_lower = needle_trim.to_ascii_lowercase();
    for e in entries {
        if let Some(id) = e["id"].as_str() {
            if id.to_ascii_lowercase() == needle_lower {
                return Some(id.to_string());
            }
        }
    }

    // 3. DOI alias: bare DOI or `doi:…` (API often stores id as `doi:10.x/y`)
    if looks_like_doi_token(needle_trim) || needle_trim.to_ascii_lowercase().starts_with("doi:") {
        let want = normalize_doi_token(needle_trim);
        let want_id = format!("doi:{want}");
        for e in entries {
            if let Some(id) = e["id"].as_str() {
                if id.to_ascii_lowercase() == want_id || normalize_doi_token(id) == want {
                    return Some(id.to_string());
                }
            }
            if let Some(doi) = entry_doi(e) {
                if normalize_doi_token(&doi) == want {
                    return entry_canonical_id(e);
                }
            }
        }
    }

    None
}
