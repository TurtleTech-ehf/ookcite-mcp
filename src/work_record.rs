//! Structured work records for `validate_doi` and the DOI side of
//! `reverse_lookup`.
//!
//! The prose head and the per-field lines a tool prints are the summary:
//! an agent can read them, but an agent that wants the title as a string,
//! the first author's ORCID, or the publisher's license URL has to parse
//! them back out of English. The record below carries every field the
//! lookup returned next to a short provenance tag (`ookcite_corpus`,
//! `crossref`, `datacite`, `zenodo`, `ookcite_corpus+crossref`, ...) and a
//! coarse confidence so the structured_content of the MCP tool response
//! is the artifact an agent reads first.
//!
//! The marker helpers at the bottom embed one record into the string the
//! tool handler returns. `promote_structured_marker` in `server.rs`
//! strips the marker from the text before the response leaves the server
//! and hoists the JSON into `structured_content`, so the human-readable
//! summary a terminal prints is unchanged.

use std::sync::Arc;

use rmcp::model::JsonObject;
use serde_json::{json, Map, Value};

use crate::bibliographic::ClaimStatus;
use crate::server::DoiSource;

/// Current schema version. Bump when the shape of `fields` changes.
pub const WORK_RECORD_KIND: &str = "ookcite.work_record/v1";

/// A field name followed by the merged value and the source that provided
/// it. A missing value is still listed with `Value::Null` so an agent that
/// looks for `fields.volume` sees "unknown provenance, no value" instead
/// of a key error.
pub const FIELDS: &[&str] = &[
    "title",
    "authors",
    "year",
    "journal",
    "container",
    "booktitle",
    "volume",
    "issue",
    "pages",
    "publisher",
    "url",
    "abstract",
    "license",
    "references",
    "type",
    "entry_type",
    "orcids",
    "open_access_pdf_url",
    "retraction",
];

/// Build the structured work record for one DOI.
///
/// * `summary` is the prose the tool would otherwise have returned (still
///   shown in `content[0].text` so a human reader sees the same thing).
/// * `stored_before_merge` is the raw `/api/v1/lookup/doi` body, or `None`
///   when the DOI was answered by the publisher fall-through.
/// * `provider_record` is `{"crossref": .., "datacite": .., "zenodo": ..}`
///   as [`crate::server::Server::provider_record`] returned it.
#[allow(clippy::too_many_arguments)]
pub fn build_validate_doi_record(
    doi: &str,
    source: DoiSource,
    stored_before_merge: Option<&Value>,
    provider_record: Option<&Value>,
    merged: &Value,
    metadata_age_seconds: u64,
    from_cache: bool,
    summary: &str,
    claim_status: Option<ClaimStatus>,
) -> Value {
    let provider_kind = detect_provider_kind(provider_record);
    let source_tag = match source {
        DoiSource::Corpus => "ookcite_corpus",
        DoiSource::NotInCorpus => "not_in_corpus",
    };

    let mut fields = Map::new();
    for key in FIELDS {
        fields.insert(
            (*key).to_string(),
            build_field(
                merged,
                stored_before_merge,
                provider_record,
                provider_kind,
                key,
            ),
        );
    }

    let authors = extract_authors(stored_before_merge, provider_record, provider_kind, merged);

    let claim_json = claim_status.map(|status| match status {
        ClaimStatus::Match => "match",
        ClaimStatus::Mismatch => "mismatch",
        ClaimStatus::Incomplete => "incomplete",
    });

    let confidence = match (source, claim_status) {
        (_, Some(ClaimStatus::Mismatch)) => "low",
        (_, Some(ClaimStatus::Incomplete)) => "medium",
        (DoiSource::Corpus, _) if provider_record.is_some() => "high",
        (DoiSource::Corpus, _) => "medium",
        (DoiSource::NotInCorpus, _) => "medium",
    };

    let warnings = detect_consistency_warnings(stored_before_merge, provider_record);

    json!({
        "kind": WORK_RECORD_KIND,
        "doi": doi,
        "source": source_tag,
        "confidence": confidence,
        "metadata_age_seconds": metadata_age_seconds,
        "from_cache": from_cache,
        "summary": summary,
        "claim_status": claim_json,
        "authors": authors,
        "fields": Value::Object(fields),
        "warnings": warnings,
    })
}

/// Flag inconsistencies between the OokCite corpus record and the
/// provider record for the same DOI. The provider is the registrar: when
/// the two disagree about author families or title, the corpus record
/// has probably mixed fields from two different works.
pub fn detect_consistency_warnings(stored: Option<&Value>, provider: Option<&Value>) -> Value {
    let Some((stored, provider)) = stored.zip(provider) else {
        return Value::Array(Vec::new());
    };
    let mut warnings: Vec<Value> = Vec::new();

    let stored_authors = author_families(stored.get("authors"));
    let provider_authors = provider_author_families(provider);
    if !stored_authors.is_empty()
        && !provider_authors.is_empty()
        && !any_shared_family(&stored_authors, &provider_authors)
    {
        warnings.push(json!({
            "code": "corpus_authors_disagree_with_provider",
            "detail": format!(
                "the corpus lists {stored_authors:?} as authors but the registrar lists {provider_authors:?}; the corpus record likely mixes fields from two different works"
            ),
        }));
    }

    let stored_title = stored
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let provider_title_text = provider_title(provider);
    if let (Some(stored_title), Some(provider_title_text)) = (stored_title, provider_title_text) {
        if !stored_title.eq_ignore_ascii_case(provider_title_text.trim()) {
            // A title-casing or punctuation rewrite is not evidence of
            // mixing. Only a fully distinct title earns the warning, and
            // an empty token set (a short title, or one whose words are
            // all three letters or fewer) is not a disagreement.
            let stored_tokens = significant_title_tokens(&stored_title);
            let provider_tokens = significant_title_tokens(&provider_title_text);
            if !stored_tokens.is_empty()
                && !provider_tokens.is_empty()
                && stored_tokens.is_disjoint(&provider_tokens)
            {
                warnings.push(json!({
                    "code": "corpus_title_disagrees_with_provider",
                    "detail": format!(
                        "the corpus title {stored_title:?} shares no significant words with the registrar title {provider_title_text:?}"
                    ),
                }));
            }
        }
    }
    Value::Array(warnings)
}

/// Words long enough to identify a title, with punctuation stripped so
/// `Lattice.` and `Lattice` are the same token. One comparison covers
/// both titles.
fn significant_title_tokens(title: &str) -> std::collections::HashSet<String> {
    title
        .split_whitespace()
        .filter_map(|token| {
            let bare: String = token
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .flat_map(|ch| ch.to_lowercase())
                .collect();
            if bare.chars().count() > 3 {
                Some(bare)
            } else {
                None
            }
        })
        .collect()
}

fn author_families(value: Option<&Value>) -> Vec<String> {
    let Some(array) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|author| author.get("family").and_then(Value::as_str))
        .map(|family| family.trim().to_ascii_lowercase())
        .filter(|family| !family.is_empty())
        .collect()
}

fn provider_author_families(provider: &Value) -> Vec<String> {
    if let Some(crossref_authors) = provider
        .get("crossref")
        .and_then(|value| value.get("author"))
        .and_then(Value::as_array)
    {
        return crossref_authors
            .iter()
            .filter_map(|author| author.get("family").and_then(Value::as_str))
            .map(|family| family.trim().to_ascii_lowercase())
            .filter(|family| !family.is_empty())
            .collect();
    }
    if let Some(datacite_creators) = provider
        .get("datacite")
        .and_then(|value| value.get("creators"))
        .and_then(Value::as_array)
    {
        return datacite_creators
            .iter()
            .filter_map(|creator| creator.get("familyName").and_then(Value::as_str))
            .map(|family| family.trim().to_ascii_lowercase())
            .filter(|family| !family.is_empty())
            .collect();
    }
    Vec::new()
}

fn provider_title(provider: &Value) -> Option<String> {
    if let Some(title) = provider
        .get("crossref")
        .and_then(|value| value.get("title"))
        .and_then(Value::as_array)
        .and_then(|array| array.first())
        .and_then(Value::as_str)
    {
        return Some(title.trim().to_string());
    }
    if let Some(title) = provider
        .get("datacite")
        .and_then(|value| value.get("titles"))
        .and_then(Value::as_array)
        .and_then(|array| array.first())
        .and_then(|first| first.get("title"))
        .and_then(Value::as_str)
    {
        return Some(title.trim().to_string());
    }
    None
}

fn any_shared_family(stored: &[String], provider: &[String]) -> bool {
    let provider_set: std::collections::HashSet<&String> = provider.iter().collect();
    stored.iter().any(|family| provider_set.contains(family))
}

/// Build a work record for a failed call. The shape matches the
/// success path (`kind`, `doi`, `summary`), so a reader can parse the
/// record the same way every time. `error.message` carries the prose a
/// human reader sees; `error.kind` names the failure family a caller
/// can switch on without string matching the message.
pub fn build_error_record(doi: &str, message: &str) -> Value {
    let message = crate::policy::redact_urls_in_text(message);
    let kind = classify_error(&message);
    json!({
        "kind": WORK_RECORD_KIND,
        "doi": doi,
        "source": "error",
        "confidence": "low",
        "summary": message,
        "error": {
            "kind": kind,
            "message": message,
        },
        "fields": Value::Object(Map::new()),
        "authors": Value::Array(Vec::new()),
        "warnings": Value::Array(Vec::new()),
    })
}

fn classify_error(message: &str) -> &'static str {
    // Prefixes are the ones `http_error::classify_lookup_doi_failure`,
    // `require_registered`, and the provider transport path actually emit.
    let head = message.lines().next().unwrap_or(message);
    if head.starts_with("INVALID") {
        "invalid_doi"
    } else if head.starts_with("RATE LIMITED") || head.starts_with("RETRY-AFTER") {
        "rate_limited"
    } else if head.starts_with("TIMEOUT") {
        "timeout"
    } else if head.starts_with("ACCESS DENIED") || head.starts_with("UNAUTHORIZED") {
        "unauthorized"
    } else if head.starts_with("PAYMENT REQUIRED") || head.starts_with("PLAN") {
        "plan_required"
    } else if head.starts_with("UNREGISTERED") {
        "unregistered"
    } else if head.starts_with("UNREACHABLE") {
        "unreachable"
    } else if head.starts_with("TEMPORARY ERROR") {
        "temporary_error"
    } else if head.starts_with("CLIENT ERROR") {
        "client_error"
    } else {
        "error"
    }
}

/// Build the structured ranked list a `reverse_lookup` call returns.
/// `candidates` is an ordered list (best first) of `(doi, title, score)`.
/// `top_confident` is true when the best score was above the server's
/// reverse-lookup confidence threshold.
#[allow(dead_code)]
pub fn build_reverse_lookup_record(
    query: &str,
    top_confident: bool,
    candidates: &[(String, String, f32)],
    summary: &str,
) -> Value {
    let candidate_array: Vec<Value> = candidates
        .iter()
        .map(|(doi, title, score)| {
            json!({
                "doi": doi,
                "title": title,
                "score": score,
            })
        })
        .collect();

    let confidence = if top_confident { "high" } else { "low" };

    json!({
        "kind": "ookcite.reverse_lookup/v1",
        "query": query,
        "confidence": confidence,
        "candidates": candidate_array,
        "summary": summary,
    })
}

fn detect_provider_kind(provider_record: Option<&Value>) -> Option<&'static str> {
    let object = provider_record.and_then(|value| value.as_object())?;
    if object.contains_key("crossref") {
        return Some("crossref");
    }
    if object.contains_key("datacite") {
        return Some("datacite");
    }
    if object.contains_key("zenodo") {
        return Some("zenodo");
    }
    None
}

/// Pick a value for `key` with OokCite-first precedence: a corpus row's
/// value always wins over a provider's, and a provider value is used
/// only when the corpus had nothing for that field. The returned
/// provenance is `ookcite_corpus`, `ookcite_corpus+<provider>` (when
/// that field's values agree), the
/// provider tag alone (when the corpus had nothing, or the DOI was not
/// in the corpus and the provider answered), or `unknown`.
///
/// Values are read directly from the pre-merge sources rather than from
/// the merged record, so a merge that silently projected a provider
/// field over a corpus field (or vice versa) cannot leak here.
fn build_field(
    merged: &Value,
    stored_before_merge: Option<&Value>,
    provider_record: Option<&Value>,
    provider_kind: Option<&'static str>,
    key: &str,
) -> Value {
    let stored_value = stored_before_merge.and_then(|value| read_stored_field(value, key));
    let provider_value = provider_record.and_then(|value| read_provider_field(value, key));

    let (value, provenance) = match (stored_value, provider_value, provider_kind) {
        (Some(stored), Some(provider), Some(kind)) => {
            let provenance = if field_values_agree(key, &stored, &provider) {
                format!("ookcite_corpus+{kind}")
            } else {
                // The combined tag means the two sources agree. A
                // disagreement keeps the corpus value and does not claim
                // agreement; `warnings` names the clash.
                "ookcite_corpus".to_string()
            };
            (stored, provenance)
        }
        (Some(stored), Some(_), None) => (stored, "ookcite_corpus".to_string()),
        (Some(stored), None, _) => (stored, "ookcite_corpus".to_string()),
        (None, Some(provider), Some(kind)) => (provider, kind.to_string()),
        (None, Some(provider), None) => (provider, "unknown".to_string()),
        (None, None, _) => {
            // The field may still be present on the merged record (a
            // field a future provider or a legacy call path wrote onto
            // the merged object without appearing on either raw side).
            // Report its value rather than drop it, but mark the
            // provenance as unknown since no raw source claimed it.
            (read_merged_field(merged, key), "unknown".to_string())
        }
    };
    json!({
        "value": value,
        "provenance": provenance,
    })
}

/// Read `key` from the raw OokCite corpus row (`stored_before_merge`).
/// Returns `None` when the row has no value for the field; the caller
/// then tries the provider record.
fn read_stored_field(stored: &Value, key: &str) -> Option<Value> {
    let value = match key {
        "year" => stored
            .get("date")
            .and_then(|date| date.get("year"))
            .cloned()
            .or_else(|| stored.get("year").cloned()),
        "authors" => stored.get("authors").cloned(),
        "references" => stored.get("references").cloned(),
        "retraction" => {
            if let Some(retraction) = stored.get("retraction") {
                Some(retraction.clone())
            } else {
                stored
                    .get("update-to")
                    .cloned()
                    .map(|update| json!({ "update_to": update }))
            }
        }
        "license" => stored.get("license").cloned(),
        "orcids" => {
            let array = stored.get("authors").and_then(Value::as_array)?;
            let orcids: Vec<Value> = array
                .iter()
                .filter_map(|author| author.get("orcid").and_then(Value::as_str))
                .map(|orcid| Value::String(orcid.to_string()))
                .collect();
            if orcids.is_empty() {
                None
            } else {
                Some(Value::Array(orcids))
            }
        }
        "open_access_pdf_url" => stored.get("oa_pdf_url").cloned(),
        "container" => stored
            .get("container")
            .cloned()
            .or_else(|| stored.get("journal").cloned()),
        "abstract" => stored
            .get("abstract")
            .cloned()
            .or_else(|| stored.get("summary").cloned()),
        _ => stored.get(key).cloned(),
    };
    non_empty(value)
}

/// Read `key` straight from the merged record. Used only as a last
/// resort when neither raw source claimed the value, so the record can
/// still surface a field a legacy path wrote onto the merged object.
fn read_merged_field(meta: &Value, key: &str) -> Value {
    read_stored_field(meta, key).unwrap_or(Value::Null)
}

/// Read `key` from the raw provider record (Crossref, DataCite, or
/// Zenodo overlay). Each provider's own JSON shape is handled below:
/// Crossref holds a journal under `container-title`, DataCite holds a
/// license under `rightsList`, and so on. The returned value is the
/// provider's own representation (so an agent reading the structured
/// record can see provider punctuation and identifiers).
fn read_provider_field(provider: &Value, key: &str) -> Option<Value> {
    if let Some(crossref) = provider.get("crossref") {
        if let Some(value) = read_crossref_field(crossref, key) {
            return Some(value);
        }
    }
    if let Some(datacite) = provider.get("datacite") {
        if let Some(value) = read_datacite_field(datacite, key) {
            return Some(value);
        }
    }
    None
}

fn read_crossref_field(crossref: &Value, key: &str) -> Option<Value> {
    match key {
        "title" => non_empty(path_get(crossref, &["title", "0"])),
        "authors" => non_empty(crossref.get("author").cloned()),
        "year" => {
            let raw = path_get(crossref, &["issued", "date-parts", "0", "0"]);
            match raw {
                Some(Value::Number(_)) | Some(Value::String(_)) => raw,
                _ => None,
            }
        }
        "journal" | "container" => non_empty(path_get(crossref, &["container-title", "0"])),
        "booktitle" => non_empty(path_get(crossref, &["container-title", "0"])),
        "volume" => non_empty(crossref.get("volume").cloned()),
        "issue" => non_empty(crossref.get("issue").cloned()),
        "pages" => non_empty(crossref.get("page").cloned()),
        "publisher" => non_empty(crossref.get("publisher").cloned()),
        "url" => non_empty(crossref.get("URL").cloned()),
        "abstract" => non_empty(crossref.get("abstract").cloned()),
        "license" => {
            // Crossref license is `[{"URL": ..., "content-version": ..}]`.
            // Surface the full array so an agent can distinguish TDM
            // licences from the content-of-record licence.
            non_empty(crossref.get("license").cloned())
        }
        "references" => non_empty(crossref.get("reference").cloned()),
        "type" | "entry_type" => non_empty(crossref.get("type").cloned()),
        "orcids" => {
            let authors = crossref.get("author").and_then(Value::as_array)?;
            let orcids: Vec<Value> = authors
                .iter()
                .filter_map(|author| author.get("ORCID").and_then(Value::as_str))
                .map(|orcid| Value::String(orcid.to_string()))
                .collect();
            if orcids.is_empty() {
                None
            } else {
                Some(Value::Array(orcids))
            }
        }
        "open_access_pdf_url" => {
            // Crossref's `link` array lists each full-text URL with a
            // `content-type` (usually `application/pdf`) and a
            // `content-version`. The first PDF (with the content-of-
            // record version when present, else any PDF) is the OA PDF.
            let links = crossref.get("link").and_then(Value::as_array)?;
            let pdf = links.iter().find(|link| {
                link.get("content-type")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value == "application/pdf")
            })?;
            pdf.get("URL")
                .cloned()
                .filter(|value| value.as_str().map(|text| !text.is_empty()).unwrap_or(false))
        }
        "retraction" => {
            // Crossref lists corrections and retractions under
            // `update-to` as `[{"DOI": ..., "type": "retraction", ..}]`.
            // The caller expects a structured object so it does not
            // need to know the raw key name.
            let updates = crossref.get("update-to").and_then(Value::as_array)?;
            if updates.is_empty() {
                None
            } else {
                Some(json!({ "update_to": Value::Array(updates.clone()) }))
            }
        }
        _ => None,
    }
}

fn read_datacite_field(datacite: &Value, key: &str) -> Option<Value> {
    match key {
        "title" => non_empty(path_get(datacite, &["titles", "0", "title"])),
        "authors" => non_empty(datacite.get("creators").cloned()),
        "year" => non_empty(datacite.get("publicationYear").cloned()),
        "journal" | "container" => non_empty(path_get(datacite, &["container", "title"])),
        "publisher" => non_empty(datacite.get("publisher").cloned()),
        "url" => non_empty(datacite.get("url").cloned()),
        "abstract" => {
            let descriptions = datacite.get("descriptions").and_then(Value::as_array)?;
            let first = descriptions
                .iter()
                .find_map(|description| description.get("description").and_then(Value::as_str))?;
            Some(Value::String(first.to_string()))
        }
        "license" => non_empty(datacite.get("rightsList").cloned()),
        "type" | "entry_type" => non_empty(path_get(datacite, &["types", "resourceTypeGeneral"])),
        "orcids" => {
            let creators = datacite.get("creators").and_then(Value::as_array)?;
            let orcids: Vec<Value> = creators
                .iter()
                .filter_map(|creator| {
                    let ids = creator.get("nameIdentifiers").and_then(Value::as_array)?;
                    ids.iter()
                        .find(|id| {
                            id.get("nameIdentifierScheme")
                                .and_then(Value::as_str)
                                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ORCID"))
                        })
                        .and_then(|id| id.get("nameIdentifier").and_then(Value::as_str))
                })
                .map(|orcid| Value::String(orcid.to_string()))
                .collect();
            if orcids.is_empty() {
                None
            } else {
                Some(Value::Array(orcids))
            }
        }
        _ => None,
    }
}

fn path_get(root: &Value, path: &[&str]) -> Option<Value> {
    let mut cursor = root;
    for segment in path {
        cursor = match cursor {
            Value::Object(map) => map.get(*segment)?,
            Value::Array(array) => {
                let index = segment.parse::<usize>().ok()?;
                array.get(index)?
            }
            _ => return None,
        };
    }
    Some(cursor.clone())
}

fn field_values_agree(key: &str, stored: &Value, provider: &Value) -> bool {
    if key == "title" {
        return stored.as_str().is_some_and(|left| {
            provider
                .as_str()
                .is_some_and(|right| left.trim().eq_ignore_ascii_case(right.trim()))
        });
    }
    if key == "authors" {
        let left = person_family_set(stored);
        let right = person_family_set(provider);
        return !left.is_empty() && left == right;
    }
    stored == provider
}

fn person_family_set(value: &Value) -> std::collections::BTreeSet<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|author| {
            author
                .get("family")
                .and_then(Value::as_str)
                .or_else(|| author.get("familyName").and_then(Value::as_str))
                .or_else(|| author.get("name").and_then(Value::as_str))
                .map(|name| name.trim().to_ascii_lowercase())
                .filter(|name| !name.is_empty())
        })
        .collect()
}

fn non_empty(value: Option<Value>) -> Option<Value> {
    match value {
        None => None,
        Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::Array(array)) if array.is_empty() => None,
        Some(Value::Object(map)) if map.is_empty() => None,
        Some(other) => Some(other),
    }
}

/// Build the `authors` array on the record. Uses the OokCite corpus
/// row's authors on a corpus hit (OokCite-first) and falls back to the
/// provider's authors otherwise. ORCID identifiers from the provider
/// record are folded onto a corpus author when the two sides agree on
/// the family name, so a corpus hit with no ORCID on the local row
/// still reports the author's ORCID.
fn trimmed_author_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn extract_authors(
    stored_before_merge: Option<&Value>,
    provider_record: Option<&Value>,
    provider_kind: Option<&'static str>,
    merged: &Value,
) -> Value {
    let base = stored_before_merge
        .and_then(|stored| stored.get("authors"))
        .cloned()
        .filter(|value| {
            value
                .as_array()
                .map(|array| !array.is_empty())
                .unwrap_or(false)
        })
        .or_else(|| provider_authors(provider_record, provider_kind))
        .or_else(|| merged.get("authors").cloned())
        .unwrap_or(Value::Array(Vec::new()));

    let provider_orcids = provider_record
        .map(collect_provider_orcids)
        .unwrap_or_default();

    let Some(array) = base.as_array() else {
        return Value::Array(Vec::new());
    };
    let out: Vec<Value> = array
        .iter()
        .map(|author| {
            let mut entry = Map::new();
            // An empty DataCite `familyName` is `Some("")`. Treating that as
            // a name skips the organisational `name` (`Example Collaboration`).
            let family = trimmed_author_text(author.get("family"))
                .or_else(|| trimmed_author_text(author.get("familyName")))
                .or_else(|| trimmed_author_text(author.get("lastName")));
            let given = trimmed_author_text(author.get("given"))
                .or_else(|| trimmed_author_text(author.get("givenName")))
                .or_else(|| trimmed_author_text(author.get("firstName")));
            if let Some(family) = family.clone() {
                entry.insert("family".into(), Value::String(family));
            }
            if let Some(given) = given {
                entry.insert("given".into(), Value::String(given));
            }
            // An organisational creator has `name` and neither family nor given.
            if !entry.contains_key("family") && !entry.contains_key("given") {
                if let Some(name) = author.get("name").and_then(Value::as_str) {
                    let name = name.trim();
                    if !name.is_empty() {
                        entry.insert("family".into(), Value::String(name.to_string()));
                        entry.insert("literal".into(), Value::Bool(true));
                    }
                }
            }
            let mut orcid = author
                .get("orcid")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    author
                        .get("ORCID")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .or_else(|| {
                    // DataCite shape: `nameIdentifiers: [{
                    // nameIdentifierScheme: "ORCID", nameIdentifier: ".." }]`.
                    author
                        .get("nameIdentifiers")
                        .and_then(Value::as_array)
                        .and_then(|ids| {
                            ids.iter()
                                .find(|id| {
                                    id.get("nameIdentifierScheme")
                                        .and_then(Value::as_str)
                                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ORCID"))
                                })
                                .and_then(|id| id.get("nameIdentifier").and_then(Value::as_str))
                                .map(str::to_string)
                        })
                });
            if orcid.is_none() {
                if let Some(family) = family {
                    orcid = provider_orcids
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(&family))
                        .map(|(_, id)| id.clone());
                }
            }
            if let Some(orcid) = orcid {
                entry.insert("orcid".into(), Value::String(orcid));
            }
            if let Some(literal) = author.get("literal").and_then(Value::as_bool) {
                entry.insert("literal".into(), Value::Bool(literal));
            }
            Value::Object(entry)
        })
        .collect();
    Value::Array(out)
}

fn provider_authors(
    provider_record: Option<&Value>,
    provider_kind: Option<&'static str>,
) -> Option<Value> {
    let (record, kind) = provider_record.zip(provider_kind)?;
    match kind {
        "crossref" => record
            .get("crossref")
            .and_then(|crossref| crossref.get("author"))
            .cloned(),
        "datacite" => record
            .get("datacite")
            .and_then(|datacite| datacite.get("creators"))
            .cloned(),
        _ => None,
    }
}

/// Collect `(family_name, orcid_uri)` pairs from the provider record so
/// a corpus author with no local ORCID can pick one up from the
/// matching provider author.
fn collect_provider_orcids(provider: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(authors) = provider
        .get("crossref")
        .and_then(|crossref| crossref.get("author"))
        .and_then(Value::as_array)
    {
        for author in authors {
            let family = author
                .get("family")
                .and_then(Value::as_str)
                .map(str::to_string);
            let orcid = author
                .get("ORCID")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let (Some(family), Some(orcid)) = (family, orcid) {
                out.push((family, orcid));
            }
        }
    }
    if let Some(creators) = provider
        .get("datacite")
        .and_then(|datacite| datacite.get("creators"))
        .and_then(Value::as_array)
    {
        for creator in creators {
            let family = creator
                .get("familyName")
                .and_then(Value::as_str)
                .or_else(|| creator.get("name").and_then(Value::as_str))
                .map(str::to_string);
            let orcid = creator
                .get("nameIdentifiers")
                .and_then(Value::as_array)
                .and_then(|ids| {
                    ids.iter()
                        .find(|id| {
                            id.get("nameIdentifierScheme")
                                .and_then(Value::as_str)
                                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ORCID"))
                        })
                        .and_then(|id| id.get("nameIdentifier").and_then(Value::as_str))
                        .map(str::to_string)
                });
            if let (Some(family), Some(orcid)) = (family, orcid) {
                out.push((family, orcid));
            }
        }
    }
    out
}

/// JSON Schema (as an [`Arc<JsonObject>`]) describing the structured
/// payload a tool promotes into `structured_content`. The schema is
/// advertised on tool listings so an agent that wants to read the
/// record without parsing prose knows the field shape, their provenance
/// vocabulary, and that the payload is versioned by `kind`.
///
/// Returned as `Arc<JsonObject>` because the rmcp tool macro records it
/// at listing time and clones the `Arc` into every listing frame; a
/// fresh schema is built each call since the fields list (`FIELDS`) may
/// grow over time.
pub fn output_schema() -> Arc<JsonObject> {
    let mut field_properties = Map::new();
    for key in FIELDS {
        field_properties.insert(
            (*key).to_string(),
            json!({
                "type": "object",
                "required": ["value", "provenance"],
                "properties": {
                    "value": { "description": "The field value as it reached the structured record (null when neither OokCite nor a provider carried the field)." },
                    "provenance": {
                        "type": "string",
                        "description": "Where the value came from. `ookcite_corpus` is the OokCite corpus row, `ookcite_corpus+<provider>` is both sources agreeing, `crossref`/`datacite`/`zenodo` is a provider alone, and `unknown` means the server could not attribute the value.",
                        "examples": [
                            "ookcite_corpus",
                            "ookcite_corpus+crossref",
                            "crossref",
                            "datacite",
                            "unknown"
                        ]
                    }
                }
            }),
        );
    }

    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "OokCite work record",
        "description": "Structured artifact a validate_doi call writes to `structured_content`. The prose summary stays in the text block for a human reader, and the record below is the machine-readable view an agent reads first.",
        "type": "object",
        "required": ["kind", "doi", "source", "confidence", "summary", "authors", "fields"],
        "properties": {
            "kind": {
                "type": "string",
                "const": WORK_RECORD_KIND,
                "description": "Schema identifier. Bumped when the shape changes so an agent can skip a record whose version it does not understand."
            },
            "doi": { "type": "string", "description": "The DOI the lookup resolved, or the DOI the caller asked about on the error path." },
            "source": {
                "type": "string",
                "enum": ["ookcite_corpus", "not_in_corpus", "error"],
                "description": "`ookcite_corpus` is a corpus hit. `not_in_corpus` is a registered DOI the OokCite corpus has no entry for; the provider record is still returned. `error` is the failure envelope: the lookup could not complete and `error` is populated."
            },
            "confidence": {
                "type": "string",
                "enum": ["low", "medium", "high"],
                "description": "Coarse confidence in the whole record. `low` when a claim did not match or the lookup failed. `high` when the corpus and a provider both answered. `medium` otherwise."
            },
            "metadata_age_seconds": {
                "type": "integer",
                "description": "Seconds since this server retrieved the record, not since publication. Absent on the error path."
            },
            "from_cache": {
                "type": "boolean",
                "description": "True when the record was served from this process's identity-safe cache rather than fetched again. Absent on the error path."
            },
            "error": {
                "type": "object",
                "description": "Populated only when `source` is `error`. `kind` matches the prefix the server emits: invalid_doi (INVALID), rate_limited (RATE LIMITED), timeout (TIMEOUT), unauthorized (ACCESS DENIED), plan_required (PAYMENT REQUIRED), unregistered (UNREGISTERED), unreachable (UNREACHABLE), temporary_error (TEMPORARY ERROR), client_error (CLIENT ERROR), or error. `message` is the prose the summary also carries, with userinfo and query strings removed from any URL.",
                "required": ["kind", "message"],
                "properties": {
                    "kind": { "type": "string" },
                    "message": { "type": "string" }
                }
            },
            "warnings": {
                "type": "array",
                "description": "Consistency warnings the server raised while assembling the record (for example, the corpus and the registrar disagree on author families for this DOI). Each entry carries a `code` and a human-readable `detail`.",
                "items": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "string" },
                        "detail": { "type": "string" }
                    }
                }
            },
            "claim_status": {
                "type": ["string", "null"],
                "enum": ["match", "mismatch", "incomplete", null],
                "description": "`match` when every claim field agreed with the record, `mismatch` on any disagreement, `incomplete` when the record did not carry a claimed field. Null when the call carried no claim. Absent on the error path."
            },
            "summary": { "type": "string", "description": "The prose a human reader sees in `content[0].text`." },
            "authors": {
                "type": "array",
                "description": "Author list with family, given, optional ORCID, and `literal` flag. On a corpus hit the corpus author list wins; a provider ORCID may be folded in when the two sides agree on the family name.",
                "items": {
                    "type": "object",
                    "properties": {
                        "family": { "type": "string" },
                        "given": { "type": "string" },
                        "orcid": { "type": "string" },
                        "literal": { "type": "boolean" }
                    }
                }
            },
            "fields": {
                "type": "object",
                "description": "One value+provenance pair per supported field. Missing values are reported as `null` with `unknown` provenance rather than dropped.",
                "properties": Value::Object(field_properties.clone()),
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    });

    Arc::new(
        schema
            .as_object()
            .cloned()
            .expect("schema literal is an object"),
    )
}

/// Begin marker that a tool handler prepends to the record JSON.
pub const STRUCTURED_MARKER_BEGIN: &str = "<<<OOKCITE_STRUCTURED_JSON_BEGIN>>>";
/// End marker. The text between the two markers must be one valid JSON
/// value. [`crate::server::promote_structured_marker`] pulls that JSON out
/// of the tool text before the response leaves the server.
pub const STRUCTURED_MARKER_END: &str = "<<<OOKCITE_STRUCTURED_JSON_END>>>";

/// Append a structured marker block to `summary`. The leading newline
/// keeps the prose terminated so a reader that ignores the marker still
/// sees a clean trailing line.
pub fn append_structured_marker(summary: &str, record: &Value) -> String {
    let serialized = serde_json::to_string(record)
        .unwrap_or_else(|_| "{\"kind\":\"ookcite.work_record/v1\",\"error\":\"serialize\"}".into());
    format!("{summary}\n{STRUCTURED_MARKER_BEGIN}\n{serialized}\n{STRUCTURED_MARKER_END}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bibliographic::ClaimStatus;

    #[test]
    fn a_corpus_hit_with_a_crossref_overlay_tags_each_field_with_both_sources() {
        let stored = serde_json::json!({
            "title": "A paper",
            "authors": [{"family": "Smith", "given": "A."}],
            "date": {"year": 2024},
            "journal": "Phys. Rev.",
            "doi": "10.5555/x"
        });
        let provider = serde_json::json!({
            "crossref": {
                "title": ["A paper"],
                "author": [{"family": "Smith", "given": "A.", "ORCID": "https://orcid.org/0000-0001-0000-0000"}],
                "container-title": ["Physical Review"],
                "issued": {"date-parts": [[2024]]},
                "volume": "10",
                "page": "1-9",
                "publisher": "APS",
                "URL": "https://doi.org/10.5555/x",
                "type": "journal-article"
            }
        });
        let merged = crate::bib_export::merge_provider_metadata(&stored, &provider);

        let record = build_validate_doi_record(
            "10.5555/x",
            DoiSource::Corpus,
            Some(&stored),
            Some(&provider),
            &merged,
            12,
            false,
            "VALID\n...",
            Some(ClaimStatus::Match),
        );

        assert_eq!(record["kind"], "ookcite.work_record/v1");
        assert_eq!(record["source"], "ookcite_corpus");
        assert_eq!(record["confidence"], "high");
        assert_eq!(record["metadata_age_seconds"], 12);
        assert_eq!(record["from_cache"], false);
        assert_eq!(record["claim_status"], "match");
        assert_eq!(
            record["fields"]["title"]["provenance"],
            "ookcite_corpus+crossref"
        );
        assert_eq!(
            record["fields"]["authors"]["provenance"],
            "ookcite_corpus+crossref"
        );
        assert_eq!(
            record["fields"]["volume"]["provenance"], "crossref",
            "volume was not in the corpus record so the provider owns it"
        );
        assert_eq!(
            record["fields"]["year"]["provenance"], "ookcite_corpus+crossref",
            "a year both sides carry as the same number is agreement"
        );
    }

    #[test]
    fn blank_provider_families_do_not_count_as_agreement() {
        let stored = serde_json::json!({
            "doi": "10.5555/agree.authors.1",
            "title": "A shared paper about lattice transport",
            "authors": [{"family": "Keeper"}],
            "date": {"year": 2024}
        });
        let provider = serde_json::json!({
            "crossref": {
                "title": ["A shared paper about lattice transport"],
                "author": [{"family": ""}],
                "issued": {"date-parts": [[2024]]},
                "type": "journal-article"
            }
        });
        let record = build_validate_doi_record(
            "10.5555/agree.authors.1",
            DoiSource::Corpus,
            Some(&stored),
            Some(&provider),
            &stored,
            0,
            false,
            "VALID",
            None,
        );
        assert_eq!(
            record["fields"]["authors"]["provenance"], "ookcite_corpus",
            "an empty provider family is not the same byline"
        );
    }

    #[test]
    fn a_crossref_family_survives_when_the_merged_row_has_no_authors() {
        let provider = serde_json::json!({
            "crossref": {
                "author": [{"family": "Keeper", "given": "K."}]
            }
        });
        let record = build_validate_doi_record(
            "10.5555/cr.names.1",
            DoiSource::NotInCorpus,
            None,
            Some(&provider),
            &serde_json::json!({"doi": "10.5555/cr.names.1"}),
            0,
            false,
            "NOT-IN-CORPUS",
            None,
        );
        assert_eq!(record["authors"][0]["family"], "Keeper");
        assert_eq!(record["authors"][0]["given"], "K.");
    }

    #[test]
    fn a_personal_name_is_not_replaced_by_a_name_field() {
        let provider = serde_json::json!({
            "datacite": {
                "creators": [{
                    "familyName": "Keeper",
                    "name": "Should Not Replace"
                }]
            }
        });
        let record = build_validate_doi_record(
            "10.5555/dc.personal.1",
            DoiSource::NotInCorpus,
            None,
            Some(&provider),
            &serde_json::json!({"doi": "10.5555/dc.personal.1"}),
            0,
            false,
            "NOT-IN-CORPUS",
            None,
        );
        assert_eq!(record["authors"][0]["family"], "Keeper");
        assert!(record["authors"][0].get("literal").is_none());
    }

    #[test]
    fn a_not_in_corpus_record_tags_every_field_with_the_provider() {
        let provider = serde_json::json!({
            "crossref": {
                "title": ["Only at the publisher"],
                "author": [{"family": "Jones", "given": "B."}],
                "container-title": ["Journal of Fixtures"],
                "issued": {"date-parts": [[2026]]},
                "publisher": "Example Press",
                "type": "journal-article"
            }
        });
        let merged = crate::bib_export::merge_provider_metadata(
            &serde_json::json!({"doi": "10.5555/y"}),
            &provider,
        );
        let record = build_validate_doi_record(
            "10.5555/y",
            DoiSource::NotInCorpus,
            None,
            Some(&provider),
            &merged,
            0,
            false,
            "NOT-IN-CORPUS\n...",
            None,
        );
        assert_eq!(record["source"], "not_in_corpus");
        assert_eq!(record["fields"]["title"]["provenance"], "crossref");
        assert_eq!(record["fields"]["publisher"]["provenance"], "crossref");
        assert_eq!(record["fields"]["year"]["provenance"], "crossref");
    }

    #[test]
    fn a_mismatched_claim_lowers_the_confidence() {
        let merged = serde_json::json!({"title": "X", "doi": "10.5555/z"});
        let record = build_validate_doi_record(
            "10.5555/z",
            DoiSource::Corpus,
            Some(&merged),
            None,
            &merged,
            0,
            false,
            "MISMATCH\n...",
            Some(ClaimStatus::Mismatch),
        );
        assert_eq!(record["confidence"], "low");
        assert_eq!(record["claim_status"], "mismatch");
    }

    #[test]
    fn provider_orcid_abstract_license_and_references_reach_the_structured_record() {
        // The OokCite row carries only the fields OokCite indexed. The
        // Crossref payload carries ORCID per author, the abstract, the
        // licence array (both TDM and VoR URLs), the reference list,
        // an OA PDF link, and an `update-to` retraction entry. The
        // structured record must surface every one.
        let stored = serde_json::json!({
            "doi": "10.5555/structured.rich.1",
            "title": "A paper that OokCite knows nothing extra about",
            "authors": [{"family": "Example", "given": "A."}],
            "date": {"year": 2026}
        });
        let provider = serde_json::json!({
            "crossref": {
                "DOI": "10.5555/structured.rich.1",
                "title": ["A paper that OokCite knows nothing extra about"],
                "author": [{
                    "family": "Example", "given": "A.",
                    "ORCID": "https://orcid.org/0000-0002-5555-0001"
                }],
                "container-title": ["Journal of Fixtures"],
                "issued": {"date-parts": [[2026]]},
                "abstract": "<jats:p>An abstract from Crossref.</jats:p>",
                "license": [
                    {"URL": "https://creativecommons.org/licenses/by/4.0/",
                     "content-version": "vor"}
                ],
                "reference": [
                    {"key": "ref1", "DOI": "10.5555/structured.rich.1.ref1"},
                    {"key": "ref2", "DOI": "10.5555/structured.rich.1.ref2"}
                ],
                "link": [
                    {"URL": "https://example.org/structured.rich.1.pdf",
                     "content-type": "application/pdf",
                     "content-version": "vor"}
                ],
                "update-to": [{
                    "DOI": "10.5555/structured.rich.1.retracted",
                    "type": "retraction"
                }],
                "type": "journal-article"
            }
        });
        let merged = crate::bib_export::merge_provider_metadata(&stored, &provider);

        let record = build_validate_doi_record(
            "10.5555/structured.rich.1",
            DoiSource::Corpus,
            Some(&stored),
            Some(&provider),
            &merged,
            0,
            false,
            "VALID",
            Some(ClaimStatus::Match),
        );

        let fields = &record["fields"];
        // Author list carries the ORCID picked up from Crossref.
        let author = record["authors"][0].clone();
        assert_eq!(
            author["orcid"].as_str(),
            Some("https://orcid.org/0000-0002-5555-0001")
        );
        // Abstract, licence, references, update-to and OA PDF URL are
        // on the record with provider provenance (the corpus row has
        // none of them).
        assert_eq!(
            fields["abstract"]["value"].as_str(),
            Some("<jats:p>An abstract from Crossref.</jats:p>")
        );
        assert_eq!(fields["abstract"]["provenance"], "crossref");
        let licenses = fields["license"]["value"].as_array().unwrap();
        assert!(licenses.iter().any(|license| license["URL"].as_str()
            == Some("https://creativecommons.org/licenses/by/4.0/")));
        assert_eq!(fields["license"]["provenance"], "crossref");
        assert_eq!(fields["references"]["value"].as_array().unwrap().len(), 2);
        assert_eq!(fields["references"]["provenance"], "crossref");
        assert_eq!(
            fields["open_access_pdf_url"]["value"].as_str(),
            Some("https://example.org/structured.rich.1.pdf")
        );
        assert_eq!(fields["open_access_pdf_url"]["provenance"], "crossref");
        assert_eq!(
            fields["retraction"]["value"]["update_to"][0]["type"].as_str(),
            Some("retraction")
        );
        // ORCID list field agrees with the per-author ORCID.
        assert_eq!(
            fields["orcids"]["value"][0].as_str(),
            Some("https://orcid.org/0000-0002-5555-0001")
        );
    }

    #[test]
    fn a_datacite_only_record_populates_every_structured_field_the_registrar_carries() {
        // A record answered by DataCite rather than Crossref (e.g. a
        // dataset, software, or preprint hosted at a DataCite
        // registrant). The structured record must surface each field
        // from the DataCite shape with provenance `datacite`.
        let provider = serde_json::json!({
            "datacite": {
                "doi": "10.5555/dc.full.1",
                "titles": [{"title": "A DataCite-only record"}],
                "creators": [{
                    "name": "Example, A.",
                    "familyName": "Example",
                    "givenName": "A.",
                    "nameIdentifiers": [{
                        "nameIdentifierScheme": "ORCID",
                        "nameIdentifier": "https://orcid.org/0000-0004-5555-0003"
                    }]
                }],
                "publicationYear": 2026,
                "publisher": "Example Repository",
                "url": "https://example.org/datasets/10.5555/dc.full.1",
                "descriptions": [{
                    "description": "A short dataset description from DataCite.",
                    "descriptionType": "Abstract"
                }],
                "rightsList": [{
                    "rights": "Creative Commons Attribution 4.0",
                    "rightsUri": "https://creativecommons.org/licenses/by/4.0/",
                    "rightsIdentifier": "CC-BY-4.0"
                }],
                "container": {"title": "Fixtures Collection"},
                "types": {"resourceTypeGeneral": "Dataset"}
            }
        });
        let merged = crate::bib_export::merge_provider_metadata(
            &serde_json::json!({"doi": "10.5555/dc.full.1"}),
            &provider,
        );
        let record = build_validate_doi_record(
            "10.5555/dc.full.1",
            DoiSource::NotInCorpus,
            None,
            Some(&provider),
            &merged,
            0,
            false,
            "NOT-IN-CORPUS",
            None,
        );
        let fields = &record["fields"];
        assert_eq!(fields["title"]["value"], "A DataCite-only record");
        assert_eq!(fields["title"]["provenance"], "datacite");
        assert_eq!(fields["year"]["provenance"], "datacite");
        assert_eq!(fields["publisher"]["provenance"], "datacite");
        assert_eq!(fields["container"]["provenance"], "datacite");
        assert_eq!(fields["url"]["provenance"], "datacite");
        assert_eq!(
            fields["abstract"]["value"],
            "A short dataset description from DataCite."
        );
        assert_eq!(fields["abstract"]["provenance"], "datacite");
        let licenses = fields["license"]["value"].as_array().unwrap();
        assert!(licenses
            .iter()
            .any(|license| license["rightsIdentifier"].as_str() == Some("CC-BY-4.0")));
        assert_eq!(fields["license"]["provenance"], "datacite");
        assert_eq!(
            fields["orcids"]["value"][0],
            "https://orcid.org/0000-0004-5555-0003"
        );
        assert_eq!(fields["orcids"]["provenance"], "datacite");
        assert_eq!(fields["type"]["value"], "Dataset");
        // Authors array carries the DataCite ORCID through the
        // provider_authors fallback.
        assert_eq!(
            record["authors"][0]["orcid"].as_str(),
            Some("https://orcid.org/0000-0004-5555-0003")
        );
        assert_eq!(record["authors"][0]["family"], "Example");
        assert_eq!(record["authors"][0]["given"], "A.");
        assert_eq!(fields["authors"]["provenance"], "datacite");
    }

    #[test]
    fn non_empty_treats_empty_strings_arrays_and_objects_as_absent() {
        // Kills non_empty match-guard mutants: each empty variant
        // must return None.
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some(serde_json::Value::Null)), None);
        assert_eq!(non_empty(Some(serde_json::Value::String("".into()))), None);
        assert_eq!(non_empty(Some(serde_json::Value::Array(Vec::new()))), None);
        assert_eq!(
            non_empty(Some(serde_json::Value::Object(serde_json::Map::new()))),
            None
        );
        assert_eq!(
            non_empty(Some(serde_json::Value::String("value".into()))),
            Some(serde_json::Value::String("value".into()))
        );
        assert_eq!(
            non_empty(Some(serde_json::Value::Bool(false))),
            Some(serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn a_corpus_hit_keeps_corpus_values_even_when_the_provider_disagrees() {
        // The OokCite row has one author and one title. Crossref lists
        // a different title and extra authors. The structured record
        // keeps the corpus title. Provenance names only the corpus when
        // the strings disagree, and the warning records the clash.
        let stored = serde_json::json!({
            "doi": "10.5555/precedence.1",
            "title": "The curated OokCite manuscript",
            "authors": [{"family": "Keeper", "given": "K."}],
            "date": {"year": 2024}
        });
        let provider = serde_json::json!({
            "crossref": {
                "DOI": "10.5555/precedence.1",
                "title": ["A different publisher headline gamma"],
                "author": [{"family": "Keeper", "given": "K."}, {"family": "Other"}],
                "container-title": ["Fixtures Weekly"],
                "issued": {"date-parts": [[2024]]},
                "type": "journal-article"
            }
        });
        let merged = crate::bib_export::merge_provider_metadata(&stored, &provider);
        let record = build_validate_doi_record(
            "10.5555/precedence.1",
            DoiSource::Corpus,
            Some(&stored),
            Some(&provider),
            &merged,
            0,
            false,
            "VALID",
            None,
        );
        let title = &record["fields"]["title"];
        assert_eq!(
            title["value"].as_str(),
            Some("The curated OokCite manuscript"),
            "a corpus title must not be replaced by a provider title"
        );
        assert_eq!(
            title["provenance"], "ookcite_corpus",
            "a disagreeing provider title must not be tagged as agreement"
        );
        let warnings = record["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| { warning["code"] == "corpus_title_disagrees_with_provider" }),
            "a distinct provider title is a warning: {warnings:?}"
        );
        let authors = record["authors"].as_array().unwrap();
        assert_eq!(
            authors.len(),
            1,
            "a corpus hit keeps the corpus author list length"
        );
    }

    #[test]
    fn corpus_authors_that_disagree_with_the_provider_for_the_same_doi_earn_a_warning() {
        // A stored record with the title+DOI of W1 and the authors of
        // W2 earns the `corpus_authors_disagree_with_provider` warning
        // so a reader can tell the corpus row mixed fields from two
        // different works.
        let stored = serde_json::json!({
            "doi": "10.5555/mixed",
            "title": "A paper",
            "authors": [{"family": "Smith", "given": "A."}]
        });
        let provider = serde_json::json!({
            "crossref": {
                "DOI": "10.5555/mixed",
                "title": ["A paper"],
                "author": [{"family": "Jones", "given": "B."}]
            }
        });
        let warnings = detect_consistency_warnings(Some(&stored), Some(&provider));
        let codes: Vec<String> = warnings
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|w| w.get("code").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        assert!(codes.contains(&"corpus_authors_disagree_with_provider".to_string()));
    }

    #[test]
    fn provider_presence_is_what_lifts_corpus_confidence_to_high() {
        // Kills a mutant that replaced the match guard
        // `provider_record.is_some()` with `true`: a corpus hit with
        // no provider overlay must stay at `medium` confidence, not
        // jump to `high`.
        let merged = serde_json::json!({"doi": "10.5555/conf.1", "title": "A paper"});

        let no_provider = build_validate_doi_record(
            "10.5555/conf.1",
            DoiSource::Corpus,
            Some(&merged),
            None,
            &merged,
            0,
            false,
            "VALID",
            None,
        );
        assert_eq!(no_provider["confidence"], "medium");

        let with_provider = build_validate_doi_record(
            "10.5555/conf.1",
            DoiSource::Corpus,
            Some(&merged),
            Some(&serde_json::json!({"crossref": {"DOI": "10.5555/conf.1", "title": ["A paper"]}})),
            &merged,
            0,
            false,
            "VALID",
            None,
        );
        assert_eq!(with_provider["confidence"], "high");
    }

    #[test]
    fn the_defaults_for_locale_and_format_name_en_us_csl() {
        // Kills mutants that replaced the default values with
        // "xyzzy" or an empty string. The locale default must be a
        // real BCP-47 tag and the format default must be `csl` for
        // the format tools to work without explicit arguments.
        assert_eq!(crate::tool_args::default_locale(), "en-US");
        assert_eq!(crate::tool_args::default_format(), "csl");
    }

    #[test]
    fn an_error_path_still_emits_a_versioned_envelope_with_a_classified_kind() {
        let record = build_error_record(
            "10.5555/does.not.exist",
            "INVALID 10.5555/does.not.exist: not registered at doi.org",
        );
        assert_eq!(record["kind"], WORK_RECORD_KIND);
        assert_eq!(record["doi"], "10.5555/does.not.exist");
        assert_eq!(record["source"], "error");
        assert_eq!(record["confidence"], "low");
        assert_eq!(record["error"]["kind"], "invalid_doi");
        assert!(record["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("INVALID"));
        // The shape stays the same — authors, fields, warnings exist
        // but are empty on the error path.
        assert!(record["authors"].is_array());
        assert!(record["fields"].is_object());
        assert!(record["warnings"].is_array());
    }

    #[test]
    fn datacite_family_and_given_survive_when_the_merged_row_has_no_authors() {
        // The merged row is empty on purpose. Deleting the DataCite arm
        // of `provider_authors` would fall through to that empty row and
        // drop the names. `familyName` / `givenName` are the registrar keys.
        let provider = serde_json::json!({
            "datacite": {
                "creators": [{
                    "familyName": "Example",
                    "givenName": "A.",
                    "nameIdentifiers": [{
                        "nameIdentifierScheme": "ORCID",
                        "nameIdentifier": "https://orcid.org/0000-0004-5555-0003"
                    }]
                }]
            }
        });
        let merged = serde_json::json!({"doi": "10.5555/dc.names.1"});
        let record = build_validate_doi_record(
            "10.5555/dc.names.1",
            DoiSource::NotInCorpus,
            None,
            Some(&provider),
            &merged,
            0,
            false,
            "NOT-IN-CORPUS",
            None,
        );
        assert_eq!(record["authors"][0]["family"], "Example");
        assert_eq!(record["authors"][0]["given"], "A.");
        assert_eq!(
            record["authors"][0]["orcid"],
            "https://orcid.org/0000-0004-5555-0003"
        );
    }

    #[test]
    fn an_organisational_creator_name_is_a_literal_family() {
        let provider = serde_json::json!({
            "datacite": {
                "creators": [{"name": "Example Collaboration", "nameType": "Organizational"}]
            }
        });
        let record = build_validate_doi_record(
            "10.5555/dc.org.1",
            DoiSource::NotInCorpus,
            None,
            Some(&provider),
            &serde_json::json!({"doi": "10.5555/dc.org.1"}),
            0,
            false,
            "NOT-IN-CORPUS",
            None,
        );
        assert_eq!(record["authors"][0]["family"], "Example Collaboration");
        assert_eq!(record["authors"][0]["literal"], true);
    }

    #[test]
    fn an_empty_datacite_family_name_does_not_hide_the_creator_name() {
        let provider = serde_json::json!({
            "datacite": {
                "creators": [{
                    "familyName": "",
                    "name": "Example Collaboration",
                    "nameType": "Organizational"
                }]
            }
        });
        let record = build_validate_doi_record(
            "10.5555/dc.blankfam.1",
            DoiSource::NotInCorpus,
            None,
            Some(&provider),
            &serde_json::json!({"doi": "10.5555/dc.blankfam.1"}),
            0,
            false,
            "NOT-IN-CORPUS",
            None,
        );
        assert_eq!(record["authors"][0]["family"], "Example Collaboration");
        assert_eq!(record["authors"][0]["literal"], true);
    }

    #[test]
    fn title_warnings_ignore_punctuation_and_empty_token_sets() {
        let stored = serde_json::json!({
            "doi": "10.5555/title.warn.1",
            "title": "On War.",
            "authors": [{"family": "Clausewitz"}]
        });
        let provider = serde_json::json!({
            "crossref": {
                "DOI": "10.5555/title.warn.1",
                "title": ["On War"],
                "author": [{"family": "Clausewitz"}]
            }
        });
        let warnings = detect_consistency_warnings(Some(&stored), Some(&provider));
        assert_eq!(warnings, serde_json::json!([]), "{warnings}");

        let punctuated = serde_json::json!({
            "title": "Lattice methods.",
            "authors": [{"family": "Keeper"}]
        });
        let punctuated_provider = serde_json::json!({
            "crossref": {
                "title": ["Lattice methods"],
                "author": [{"family": "Keeper"}]
            }
        });
        assert_eq!(
            detect_consistency_warnings(Some(&punctuated), Some(&punctuated_provider)),
            serde_json::json!([])
        );

        let distinct = serde_json::json!({
            "title": "Curated OokCite Manuscript Title Alpha",
            "authors": [{"family": "Keeper"}]
        });
        let distinct_provider = serde_json::json!({
            "crossref": {
                "title": ["Completely Different Crossref Paper Gamma"],
                "author": [{"family": "Keeper"}]
            }
        });
        let warnings = detect_consistency_warnings(Some(&distinct), Some(&distinct_provider));
        assert!(
            warnings
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == "corpus_title_disagrees_with_provider"),
            "{warnings}"
        );
        assert!(warnings[0]["detail"]
            .as_str()
            .unwrap()
            .contains("Completely Different Crossref Paper Gamma"));
    }

    #[test]
    fn agreeing_authors_and_titles_produce_no_warning() {
        let stored = serde_json::json!({
            "title": "A shared paper about lattice transport",
            "authors": [{"family": "Keeper"}, {"family": "Quinn"}]
        });
        let provider = serde_json::json!({
            "crossref": {
                "title": ["A shared paper about lattice transport"],
                "author": [{"family": "Quinn"}, {"family": "Keeper"}]
            }
        });
        assert_eq!(
            detect_consistency_warnings(Some(&stored), Some(&provider)),
            serde_json::json!([])
        );
        // A missing author list on one side is not a disagreement.
        let no_provider_authors =
            serde_json::json!({"crossref": {"title": ["A shared paper about lattice transport"]}});
        assert_eq!(
            detect_consistency_warnings(Some(&stored), Some(&no_provider_authors)),
            serde_json::json!([])
        );
        let no_stored_authors =
            serde_json::json!({"title": "A shared paper about lattice transport"});
        assert_eq!(
            detect_consistency_warnings(Some(&no_stored_authors), Some(&provider)),
            serde_json::json!([])
        );
        // An empty DataCite familyName is not an author. Keeping it would
        // look like a disjoint byline.
        let blank_family = serde_json::json!({
            "datacite": {
                "titles": [{"title": "A shared paper about lattice transport"}],
                "creators": [{"familyName": ""}]
            }
        });
        assert_eq!(
            detect_consistency_warnings(Some(&stored), Some(&blank_family)),
            serde_json::json!([])
        );
    }

    #[test]
    fn a_three_letter_word_does_not_hide_a_real_title_clash() {
        // Tokens of length 3 are ignored. Sharing "cat" must not cancel
        // two disjoint longer words, and a length check that keeps
        // three-letter words would hide the clash.
        let stored = serde_json::json!({"title": "cat abcd"});
        let provider = serde_json::json!({"crossref": {"title": ["cat wxyz"]}});
        let warnings = detect_consistency_warnings(Some(&stored), Some(&provider));
        assert!(
            warnings
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == "corpus_title_disagrees_with_provider"),
            "{warnings}"
        );
    }

    #[test]
    fn classify_error_uses_the_prefixes_the_server_emits() {
        assert_eq!(
            classify_error("INVALID 10.5555/x : HTTP 404"),
            "invalid_doi"
        );
        assert_eq!(
            classify_error("RATE LIMITED 10.5555/x : slow"),
            "rate_limited"
        );
        assert_eq!(classify_error("RETRY-AFTER: 30"), "rate_limited");
        assert_eq!(classify_error("TIMEOUT 10.5555/x : slow"), "timeout");
        assert_eq!(
            classify_error("ACCESS DENIED 10.5555/x : 401"),
            "unauthorized"
        );
        assert_eq!(classify_error("UNAUTHORIZED"), "unauthorized");
        assert_eq!(
            classify_error("PAYMENT REQUIRED 10.5555/x : 402"),
            "plan_required"
        );
        assert_eq!(classify_error("PLAN required"), "plan_required");
        assert_eq!(
            classify_error("UNREGISTERED 10.5555/x: not registered"),
            "unregistered"
        );
        assert_eq!(
            classify_error("UNREACHABLE 10.5555/x : could not connect"),
            "unreachable"
        );
        assert_eq!(
            classify_error("TEMPORARY ERROR 10.5555/x : 503"),
            "temporary_error"
        );
        assert_eq!(
            classify_error("CLIENT ERROR 10.5555/x : 400"),
            "client_error"
        );
        assert_eq!(classify_error("something else"), "error");
    }

    #[test]
    fn an_error_record_strips_userinfo_and_query_secrets() {
        let record = build_error_record(
            "10.5555/secret.url",
            "ERROR 10.5555/secret.url : error sending request for url (http://alice:hunter2@127.0.0.1:9/api/v1/lookup/doi?api_key=secret_query)",
        );
        let message = record["error"]["message"].as_str().unwrap();
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains("alice"), "{message}");
        assert!(!message.contains("secret_query"), "{message}");
        assert!(!message.contains("api_key"), "{message}");
        assert!(message.contains("127.0.0.1"), "{message}");
    }

    #[test]
    fn the_marker_round_trips_and_wraps_the_prose() {
        let record = serde_json::json!({"kind": WORK_RECORD_KIND, "doi": "10.5555/a"});
        let wrapped = append_structured_marker("VALID\nDOI: 10.5555/a", &record);
        assert!(wrapped.contains("VALID"));
        assert!(wrapped.contains(STRUCTURED_MARKER_BEGIN));
        assert!(wrapped.contains(STRUCTURED_MARKER_END));
    }
}
