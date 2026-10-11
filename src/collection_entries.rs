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
/// a single token becomes a bare family name. A name wrapped in braces, with
/// or without a trailing comma, is an organisation and stays one literal.
pub fn person_from_name(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim().trim_end_matches(',').trim();
    if let Some(name) = braced_literal(trimmed) {
        return serde_json::json!({ "family": name, "literal": true });
    }
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

fn braced_literal(value: &str) -> Option<String> {
    if value.starts_with('{') && value.ends_with('}') && value.len() > 2 {
        let inner = value[1..value.len() - 1].trim();
        if inner.is_empty() {
            None
        } else {
            Some(inner.to_string())
        }
    } else {
        None
    }
}

/// Overlay the fields a caller supplied onto an entry's existing metadata.
///
/// The API replaces the whole `CitationMetadata` object, and that object has
/// required fields (`id`, `entry_type`, `title`, `authors`) a partial edit does
/// not carry. Starting from the stored metadata keeps those intact so a caller
/// correcting one wrong year does not blank the rest of the record.
///
/// An empty string, an empty author list, or a name in `clear` removes that
/// key. The replacement object then omits it, which is how a field is deleted.
/// A non-empty value is stored and recorded in `user_fields` so a later export
/// keeps the edit. A cleared field is not locked.
pub fn apply_entry_metadata_overrides(
    current: &serde_json::Value,
    args: &UpdateEntryMetadataArgs,
) -> serde_json::Value {
    let mut meta = current.as_object().cloned().unwrap_or_default();

    if let Some(names) = &args.clear {
        for name in names {
            let key = name.trim();
            if key.is_empty() {
                continue;
            }
            if key.eq_ignore_ascii_case("year") {
                clear_year(&mut meta);
            } else {
                meta.remove(key);
            }
            unlock_field(&mut meta, key);
        }
    }

    let mut assign = |key: &str, value: &Option<String>| {
        let Some(value) = value else {
            return;
        };
        if value.is_empty() {
            meta.remove(key);
            unlock_field(&mut meta, key);
        } else {
            meta.insert(key.into(), serde_json::json!(value));
            lock_field(&mut meta, key);
        }
    };
    assign("title", &args.title);
    assign("journal", &args.journal);
    assign("volume", &args.volume);
    assign("issue", &args.issue);
    assign("pages", &args.pages);
    assign("publisher", &args.publisher);
    assign("doi", &args.doi);
    assign("url", &args.url);
    assign("version", &args.version);
    assign("eid", &args.eid);
    assign("booktitle", &args.booktitle);
    assign("entry_type", &args.entry_type);
    assign("institution", &args.institution);
    assign("chapter", &args.chapter);
    assign("number", &args.number);
    assign("citation_key", &args.citation_key);
    assign("howpublished", &args.howpublished);
    assign("note", &args.note);

    if let Some(names) = &args.authors {
        let people: Vec<serde_json::Value> = names
            .iter()
            .filter(|name| !name.trim().is_empty())
            .map(|name| person_from_name(name))
            .collect();
        if people.is_empty() {
            meta.remove("authors");
            unlock_field(&mut meta, "authors");
        } else {
            meta.insert("authors".into(), serde_json::Value::Array(people));
            lock_field(&mut meta, "authors");
        }
    }
    if let Some(names) = &args.editor {
        let people: Vec<serde_json::Value> = names
            .iter()
            .filter(|name| !name.trim().is_empty())
            .map(|name| person_from_name(name))
            .collect();
        if people.is_empty() {
            meta.remove("editor");
            unlock_field(&mut meta, "editor");
        } else {
            meta.insert("editor".into(), serde_json::Value::Array(people));
            lock_field(&mut meta, "editor");
        }
    }
    if let Some(year) = args.year {
        // `date` is a DateValue object; keep any month/day already recorded.
        let mut date = meta
            .get("date")
            .and_then(|d| d.as_object().cloned())
            .unwrap_or_default();
        date.insert("year".into(), serde_json::json!(year));
        meta.insert("date".into(), serde_json::Value::Object(date));
        lock_field(&mut meta, "year");
    }

    serde_json::Value::Object(meta)
}

fn clear_year(meta: &mut serde_json::Map<String, serde_json::Value>) {
    meta.remove("year");
    let drop_date = meta
        .get("date")
        .and_then(|date| date.as_object())
        .is_some_and(|date| date.len() == 1 && date.contains_key("year"));
    if drop_date {
        meta.remove("date");
        return;
    }
    if let Some(date) = meta.get_mut("date").and_then(|date| date.as_object_mut()) {
        date.remove("year");
    }
}

fn lock_field(meta: &mut serde_json::Map<String, serde_json::Value>, field: &str) {
    let mut fields = meta
        .get("user_fields")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if !fields.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(field))
    }) {
        fields.push(serde_json::Value::String(field.to_string()));
    }
    meta.insert("user_fields".into(), serde_json::Value::Array(fields));
}

fn unlock_field(meta: &mut serde_json::Map<String, serde_json::Value>, field: &str) {
    let Some(fields) = meta
        .get_mut("user_fields")
        .and_then(|value| value.as_array_mut())
    else {
        return;
    };
    fields.retain(|value| {
        !value
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(field))
    });
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
        || args.version.is_some()
        || args.eid.is_some()
        || args.booktitle.is_some()
        || args.entry_type.is_some()
        || args.institution.is_some()
        || args.chapter.is_some()
        || args.number.is_some()
        || args.citation_key.is_some()
        || args.howpublished.is_some()
        || args.note.is_some()
        || args.editor.is_some()
        || args
            .clear
            .as_ref()
            .is_some_and(|names| names.iter().any(|name| !name.trim().is_empty()))
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
