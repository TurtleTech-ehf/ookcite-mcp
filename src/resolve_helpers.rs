//! Reverse-lookup and free-text resolve helpers.

use tokio::time::{sleep, Duration};

use crate::constants::rate_limit_hint;
use crate::http_error::error_detail;
use crate::tool_args::ReverseArgs;
use ookcite_mcp::endpoints;

pub struct ReverseLookupMatch {
    pub output: String,
    pub top_score: f64,
}

pub fn format_reverse_lookup_payload(payload: &serde_json::Value) -> Option<ReverseLookupMatch> {
    // /api/v1/reverse returns a bare array; /api/v1/resolve returns an object
    // with candidates/paper. Accept both so reverse_lookup can use reverse.
    let array_candidates: Vec<serde_json::Value> = if let Some(arr) = payload.as_array() {
        arr.clone()
    } else {
        payload
            .get("candidates")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    let mut top_score = f64::NEG_INFINITY;
    if let Some(paper) = payload.get("paper") {
        let title = paper["title"].as_str().unwrap_or("?");
        let doi = paper["doi"].as_str().unwrap_or("?");
        let journal = paper["journal"].as_str().unwrap_or("N/A");
        let authors = format_author_list(paper);
        out.push(format!(
            "1. [score:100] {title} | {authors} | {journal} (doi:{doi})"
        ));
        top_score = top_score.max(100.0);
    }
    let offset = out.len();
    for (i, c) in array_candidates.iter().enumerate() {
        let meta = c.get("metadata").unwrap_or(c);
        let title = meta["title"].as_str().unwrap_or("?");
        let doi = meta["doi"].as_str().unwrap_or("?");
        let journal = meta["journal"].as_str().unwrap_or("N/A");
        let authors = format_author_list(meta);
        let score = c["score"].as_f64().unwrap_or(0.0);
        top_score = top_score.max(score);
        out.push(format!(
            "{}. [score:{:.0}] {title} | {authors} | {journal} (doi:{doi})",
            offset + i + 1,
            score
        ));
    }
    if out.is_empty() {
        None
    } else {
        Some(ReverseLookupMatch {
            output: out.join("\n"),
            top_score,
        })
    }
}

fn format_author_list(meta: &serde_json::Value) -> String {
    let Some(authors) = meta.get("authors").and_then(|a| a.as_array()) else {
        return "Unknown author".into();
    };
    let names: Vec<String> = authors
        .iter()
        .take(3)
        .map(|a| {
            let given = a["given"].as_str().unwrap_or("").trim();
            let family = a["family"].as_str().unwrap_or("").trim();
            format!("{given} {family}").trim().to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        "Unknown author".into()
    } else {
        names.join("; ")
    }
}

pub async fn classify_reverse_lookup_response(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<Option<ReverseLookupMatch>, String> {
    match response {
        Ok(resp) if resp.status().is_success() => {
            let payload: serde_json::Value = resp.json().await.unwrap_or_default();
            Ok(format_reverse_lookup_payload(&payload))
        }
        Ok(r) if r.status().as_u16() == 429 => Err(format!(
            "RATE LIMITED: {}\n{}",
            error_detail(r).await,
            rate_limit_hint()
        )),
        Ok(r) if r.status().as_u16() == 403 => {
            Err(format!("ACCESS DENIED: {}", error_detail(r).await))
        }
        Ok(r) if r.status().as_u16() == 504 || r.status().as_u16() == 408 => {
            Err(format!("TIMEOUT: {}", error_detail(r).await))
        }
        Ok(r) if r.status().is_server_error() => {
            Err(format!("TEMPORARY ERROR: {}", error_detail(r).await))
        }
        Ok(_) => Ok(None),
        Err(e) => Err(format!("Reverse lookup failed: {e}")),
    }
}

/// Body for `/api/v1/reverse` — folds structured filters into free text so
/// author/orcid/year hints still bias the lexical search (same approach as
/// the OokCite web client).
pub fn reverse_lookup_body(args: &ReverseArgs) -> serde_json::Value {
    let mut parts = vec![args.text.trim().to_string()];
    if let Some(author) = &args.author {
        if !author.trim().is_empty() {
            parts.push(author.trim().to_string());
        }
    }
    if let Some(journal) = &args.journal {
        if !journal.trim().is_empty() {
            parts.push(journal.trim().to_string());
        }
    }
    if let Some(year) = args.year {
        parts.push(year.to_string());
    }
    if let Some(orcid) = &args.orcid {
        if !orcid.trim().is_empty() {
            parts.push(orcid.trim().to_string());
        }
    }
    serde_json::json!({
        "text": parts.join(" ").trim(),
        "use_neural": false
    })
}

/// Body for `/api/v1/resolve` when structured filters + live queries are needed.
pub fn reverse_lookup_resolve_body(
    args: &ReverseArgs,
    use_live_queries: bool,
) -> serde_json::Value {
    let mut body = resolve_text_body(&args.text, use_live_queries);
    let mut filters = serde_json::Map::new();
    if let Some(author) = &args.author {
        filters.insert("author".into(), serde_json::json!(author));
    }
    if let Some(journal) = &args.journal {
        filters.insert("journal".into(), serde_json::json!(journal));
    }
    if let Some(year) = args.year {
        filters.insert("year".into(), serde_json::json!(year));
    }
    if let Some(orcid) = &args.orcid {
        filters.insert("orcid".into(), serde_json::json!(orcid));
    }
    if !filters.is_empty() {
        body["filters"] = serde_json::Value::Object(filters);
    }
    body
}

pub fn format_resolve_candidates(query: &str, candidates: &[serde_json::Value]) -> String {
    let mut lines = vec![format!("Ambiguous match for '{}'. Top candidates:", query)];
    for (idx, candidate) in candidates.iter().take(5).enumerate() {
        let meta = candidate.get("metadata").unwrap_or(candidate);
        let title = meta["title"].as_str().unwrap_or("?");
        let doi = meta["doi"].as_str().unwrap_or("?");
        let year = meta["date"]["year"]
            .as_i64()
            .map(|y| format!(" ({y})"))
            .unwrap_or_default();
        let journal = meta["journal"].as_str().unwrap_or("N/A");
        lines.push(format!(
            "{}. {}{} | {} | doi:{}",
            idx + 1,
            title,
            year,
            journal,
            doi
        ));
    }
    lines.join("\n")
}

pub fn resolve_payload_metadata(payload: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(paper) = payload.get("paper").cloned() {
        return Some(paper);
    }

    let verified = payload
        .get("verification")
        .and_then(|value| value.get("status"))
        .and_then(|value| value.as_str())
        .is_some_and(|status| status.eq_ignore_ascii_case("verified"));

    if !verified {
        return None;
    }

    payload
        .get("candidates")
        .and_then(|value| value.as_array())
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("metadata"))
        .cloned()
}

/// The DOI a metadata object names, lowercased and bare.
pub fn metadata_doi(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("doi")
        .and_then(|value| value.as_str())
        .map(|doi| {
            doi.trim()
                .to_ascii_lowercase()
                .trim_start_matches("doi:")
                .to_string()
        })
        .filter(|doi| !doi.is_empty())
}

/// Whether the resolver's answer survives comparison with the ranked
/// candidates from `/api/v1/reverse`.
///
/// The two paths used to answer independently: whatever `/resolve`
/// returned was taken, and `/reverse` was consulted only when it
/// returned nothing. So a citation could format as a paper the ranker
/// placed third while its own top hit scored more than twice as high.
///
/// A resolver answer is kept when the ranker did not run, returned
/// nothing, or lists that DOI anywhere in its candidates. It loses only
/// when the ranker produced a set that does not contain it at all: an
/// answer absent from the ranked set was never ranked.
pub fn resolver_answer_agrees_with_ranking(
    resolved: &serde_json::Value,
    ranked: &[serde_json::Value],
) -> bool {
    if ranked.is_empty() {
        return true;
    }
    let Some(resolved_doi) = metadata_doi(resolved) else {
        return true;
    };
    ranked
        .iter()
        .filter_map(|item| item.get("metadata").and_then(metadata_doi))
        .any(|doi| doi == resolved_doi)
}

pub fn resolve_text_body(query: &str, use_live_queries: bool) -> serde_json::Value {
    serde_json::json!({
        "input": { "kind": "text", "text": query },
        "filters": {},
        "options": {
            "max_candidates": 5,
            "prefer_exact_identifier": true,
            "use_live_queries": use_live_queries
        }
    })
}

/// Body for `POST /api/v1/resolve/batch` in synchronous mode.
///
/// Each citation becomes one `ResolveRequest` with the same options
/// `resolve_text_body` uses for the single-citation path, so a batch and a
/// sequence of single calls rank identically. The `async` flag is left off:
/// async mode answers with a job id, and this server exposes no job-polling
/// tool to collect one.
pub fn batch_resolve_request_body(
    citations: &[String],
    use_live_queries: bool,
) -> serde_json::Value {
    let inputs: Vec<serde_json::Value> = citations
        .iter()
        .map(|c| resolve_text_body(c, use_live_queries))
        .collect();
    serde_json::json!({ "inputs": inputs })
}

/// One agent-facing line per input, in the order the caller supplied them.
///
/// The API returns each result tagged with its `index` in the request array and
/// failures do not abort the batch, so an item missing from `results` is
/// reported rather than silently dropped.
pub fn format_batch_resolve_results(
    citations: &[String],
    payload: &serde_json::Value,
) -> Vec<String> {
    let results = payload["results"].as_array().cloned().unwrap_or_default();
    let mut by_index: std::collections::HashMap<usize, &serde_json::Value> =
        std::collections::HashMap::new();
    for r in &results {
        if let Some(idx) = r["index"].as_u64() {
            by_index.insert(idx as usize, r);
        }
    }
    citations
        .iter()
        .enumerate()
        .map(|(i, citation)| {
            let n = i + 1;
            let Some(result) = by_index.get(&i) else {
                return format!("{n}. NO RESULT | {citation}");
            };
            if result["status"].as_str() == Some("error") {
                let msg = result["message"].as_str().unwrap_or("resolve failed");
                return format!("{n}. ERROR | {citation} | {msg}");
            }
            match resolve_payload_metadata(result) {
                Some(meta) => {
                    let title = meta["title"].as_str().unwrap_or("?");
                    let doi = meta["doi"].as_str().unwrap_or("?");
                    let journal = meta["journal"].as_str().unwrap_or("N/A");
                    let year = meta["date"]["year"]
                        .as_i64()
                        .map(|y| format!(" ({y})"))
                        .unwrap_or_default();
                    let authors = format_author_list(&meta);
                    format!("{n}. RESOLVED | {title}{year} | {authors} | {journal} | doi:{doi}")
                }
                None => format!("{n}. NO CONFIDENT MATCH | {citation}"),
            }
        })
        .collect()
}

fn is_retryable_lookup_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 502..=504)
}

pub async fn lookup_doi_with_retry(
    http: &reqwest::Client,
    api_base: &str,
    doi: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut attempt = 0u8;
    loop {
        let response = http
            .post(endpoints::LOOKUP_DOI.url(api_base, &[]))
            .json(&serde_json::json!({ "doi": doi }))
            .send()
            .await?;
        let status = response.status();
        if attempt < 2 && is_retryable_lookup_status(status) {
            attempt += 1;
            sleep(Duration::from_millis(150 * u64::from(attempt))).await;
            continue;
        }
        return Ok(response);
    }
}

#[cfg(test)]
mod tests {
    use super::is_retryable_lookup_status;
    use reqwest::StatusCode;

    #[test]
    fn lookup_retry_policy_stops_on_rate_limit() {
        assert!(!is_retryable_lookup_status(StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn lookup_retry_policy_keeps_transient_gateway_retries() {
        for status in [
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(is_retryable_lookup_status(status), "status {status}");
        }
    }
}

#[cfg(test)]
mod pooled_candidate_tests {
    use super::*;
    use serde_json::json;

    fn ranked(dois: &[&str]) -> Vec<serde_json::Value> {
        dois.iter()
            .map(|doi| json!({"metadata": {"doi": doi}}))
            .collect()
    }

    #[test]
    fn a_resolver_answer_the_ranker_also_lists_is_kept() {
        let resolved = json!({"doi": "10.1145/2009916.2010048"});
        assert!(resolver_answer_agrees_with_ranking(
            &resolved,
            &ranked(&["10.1145/3626772.3657906", "10.1145/2009916.2010048"]),
        ));
    }

    /// The shape that formatted a citation as the wrong paper: the
    /// resolver returned a record the ranker placed nowhere, while the
    /// ranker's own top hit was the paper asked for.
    #[test]
    fn a_resolver_answer_absent_from_the_ranking_loses() {
        let resolved = json!({"doi": "10.18653/v1/d19-1261"});
        assert!(!resolver_answer_agrees_with_ranking(
            &resolved,
            &ranked(&["10.1007/978-3-030-15712-8_23", "10.18653/v1/d19-1612"]),
        ));
    }

    #[test]
    fn an_empty_ranking_cannot_overrule_the_resolver() {
        let resolved = json!({"doi": "10.1145/2009916.2010048"});
        assert!(resolver_answer_agrees_with_ranking(&resolved, &[]));
    }

    #[test]
    fn an_answer_without_a_doi_is_left_alone() {
        assert!(resolver_answer_agrees_with_ranking(
            &json!({"title": "A paper with no DOI"}),
            &ranked(&["10.1145/2009916.2010048"]),
        ));
    }

    #[test]
    fn doi_comparison_ignores_case_and_the_doi_prefix() {
        let resolved = json!({"doi": "DOI:10.1145/2009916.2010048"});
        assert!(resolver_answer_agrees_with_ranking(
            &resolved,
            &ranked(&["10.1145/2009916.2010048"]),
        ));
        assert_eq!(
            metadata_doi(&json!({"doi": " doi:10.1/AB "})).as_deref(),
            Some("10.1/ab")
        );
    }
}
