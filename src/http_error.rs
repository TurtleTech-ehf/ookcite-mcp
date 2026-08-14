//! HTTP error formatting and classification for tool responses.

use crate::constants::rate_limit_hint;

/// Extract a useful error message from a failed HTTP response.
pub async fn error_detail(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    // Try to parse JSON error with message field
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(msg) = json["message"].as_str() {
            return format!("{status}: {msg}");
        }
    }
    if body.len() > 120 {
        format!("{status}: {}", &body[..120])
    } else if body.is_empty() {
        format!("{status}")
    } else {
        format!("{status}: {body}")
    }
}

pub async fn classify_lookup_doi_failure(resp: reqwest::Response, doi: &str) -> String {
    if resp.status().as_u16() == 429 {
        format!(
            "RATE LIMITED {doi} : {}\n{}",
            error_detail(resp).await,
            rate_limit_hint()
        )
    } else if matches!(resp.status().as_u16(), 401 | 403) {
        format!("ACCESS DENIED {doi} : {}", error_detail(resp).await)
    } else if resp.status().is_server_error() {
        format!("TEMPORARY ERROR {doi} : {}", error_detail(resp).await)
    } else if resp.status().is_client_error() && resp.status().as_u16() != 404 {
        format!("CLIENT ERROR {doi} : {}", error_detail(resp).await)
    } else {
        format!("INVALID {doi} : HTTP {}", resp.status())
    }
}

pub async fn classify_collection_create_failure(resp: reqwest::Response, name: &str) -> String {
    let detail = error_detail(resp).await;
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("collection limit reached") {
        format!(
            "Collection '{name}' could not be created: {detail}. Use an existing collection, upgrade your plan, or purchase additional collections."
        )
    } else if lowered.contains("plan_required") || lowered.contains("requires") {
        format!(
            "Collection '{name}' could not be created: {detail}. This workflow may require a paid plan or additional collection capacity."
        )
    } else {
        format!("Failed to create collection '{name}': {detail}")
    }
}
