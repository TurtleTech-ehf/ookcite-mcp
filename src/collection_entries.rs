//! Collection entry identity helpers (canonical ids, DOI aliases, search lines).

use crate::tool_args::UpdateEntryMetadataArgs;

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

/// Split one author string into the API's `Person` shape (`family` is required,
/// `given` is optional). Accepts both `"Family, Given"` and `"Given Family"`;
/// a single token becomes a bare family name.
pub fn person_from_name(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if let Some((family, given)) = trimmed.split_once(',') {
        let family = family.trim();
        let given = given.trim();
        if given.is_empty() {
            return serde_json::json!({ "family": family });
        }
        return serde_json::json!({ "family": family, "given": given });
    }
    match trimmed.rsplit_once(char::is_whitespace) {
        Some((given, family)) => {
            serde_json::json!({ "family": family.trim(), "given": given.trim() })
        }
        None => serde_json::json!({ "family": trimmed }),
    }
}

/// Overlay the fields a caller supplied onto an entry's existing metadata.
///
/// The API replaces the whole `CitationMetadata` object, and that object has
/// required fields (`id`, `entry_type`, `title`, `authors`) a partial edit does
/// not carry. Starting from the stored metadata keeps those intact so a caller
/// correcting one wrong year does not blank the rest of the record.
pub fn apply_entry_metadata_overrides(
    current: &serde_json::Value,
    args: &UpdateEntryMetadataArgs,
) -> serde_json::Value {
    let mut meta = current.as_object().cloned().unwrap_or_default();

    let mut set_str = |key: &str, value: &Option<String>| {
        if let Some(v) = value {
            meta.insert(key.into(), serde_json::json!(v));
        }
    };
    set_str("title", &args.title);
    set_str("journal", &args.journal);
    set_str("volume", &args.volume);
    set_str("issue", &args.issue);
    set_str("pages", &args.pages);
    set_str("publisher", &args.publisher);
    set_str("doi", &args.doi);
    set_str("url", &args.url);

    if let Some(names) = &args.authors {
        let people: Vec<serde_json::Value> = names
            .iter()
            .filter(|n| !n.trim().is_empty())
            .map(|n| person_from_name(n))
            .collect();
        meta.insert("authors".into(), serde_json::Value::Array(people));
    }
    if let Some(year) = args.year {
        // `date` is a DateValue object; keep any month/day already recorded.
        let mut date = meta
            .get("date")
            .and_then(|d| d.as_object().cloned())
            .unwrap_or_default();
        date.insert("year".into(), serde_json::json!(year));
        meta.insert("date".into(), serde_json::Value::Object(date));
    }

    serde_json::Value::Object(meta)
}

/// Whether any editable metadata field was supplied.
pub fn entry_metadata_overrides_present(args: &UpdateEntryMetadataArgs) -> bool {
    args.title.is_some()
        || args.authors.is_some()
        || args.journal.is_some()
        || args.year.is_some()
        || args.volume.is_some()
        || args.issue.is_some()
        || args.pages.is_some()
        || args.publisher.is_some()
        || args.doi.is_some()
        || args.url.is_some()
}

/// The stored metadata object for one entry of a collection body. Matches on the
/// same canonical id `resolve_entry_id_in_collection` returns, so entries that
/// carry only a DOI alias resolve here too.
pub fn entry_metadata_by_id<'a>(
    entries: &'a [serde_json::Value],
    entry_id: &str,
) -> Option<&'a serde_json::Value> {
    entries
        .iter()
        .find(|e| entry_canonical_id(e).as_deref() == Some(entry_id))
        .map(|e| &e["metadata"])
        .filter(|m| m.is_object())
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
