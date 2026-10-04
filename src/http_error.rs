//! HTTP error formatting and classification for tool responses.

use crate::constants::rate_limit_hint;

const MAX_ERROR_BODY_CHARS: usize = 120;
const MAX_RETRY_AFTER_CHARS: usize = 128;

/// Extract a useful error message from a failed HTTP response.
///
/// The status, a bounded body excerpt, and `Retry-After` when the server
/// sent one all stay in the text. A later tool result can tell a throttle
/// from a missing citation.
pub async fn error_detail(resp: reqwest::Response) -> String {
    let status = resp.status();
    let retry_after = retry_after_header(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    let mut detail = format_error_detail(status, &body);
    if let Some(retry_after) = retry_after {
        detail.push_str(" (Retry-After: ");
        detail.push_str(&retry_after);
        detail.push(')');
    }
    detail
}

pub async fn classify_lookup_doi_failure(resp: reqwest::Response, doi: &str) -> String {
    let status = resp.status();
    let detail = error_detail(resp).await;
    if status.as_u16() == 429 {
        format!("RATE LIMITED {doi} : {detail}\n{}", rate_limit_hint())
    } else if matches!(status.as_u16(), 401 | 403) {
        format!("ACCESS DENIED {doi} : {detail}")
    } else if status.is_server_error() {
        format!("TEMPORARY ERROR {doi} : {detail}")
    } else if status.is_client_error() && status.as_u16() != 404 {
        format!("CLIENT ERROR {doi} : {detail}")
    } else {
        format!("INVALID {doi} : HTTP {status}")
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

/// A status the caller must see as itself. A miss and a throttle are not
/// the same answer, and a restart's 503 is not a missing citation.
pub fn is_promoted_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500..=599)
}

/// Tool text for a failed response. Promoted statuses name the failure.
/// Anything else keeps the caller's prefix plus the response detail.
pub async fn failure_text(resp: reqwest::Response, prefix: &str) -> String {
    let status = resp.status();
    let detail = error_detail(resp).await;
    if is_promoted_status(status) {
        promoted_failure_text(status, &detail)
    } else {
        format!("{prefix}: {detail}")
    }
}

/// The operation prefix stays on every failure. A throttle or a restart
/// also keeps its own marker, so a client can tell them from a miss.
pub async fn prefixed_failure(resp: reqwest::Response, prefix: &str) -> String {
    let text = failure_text(resp, prefix).await;
    if text.starts_with(prefix) {
        text
    } else {
        format!("{prefix}: {text}")
    }
}

fn promoted_failure_text(status: reqwest::StatusCode, detail: &str) -> String {
    match status.as_u16() {
        429 => format!("RATE LIMITED: {detail}\n{}", rate_limit_hint()),
        408 | 504 => format!("TIMEOUT: {detail}"),
        500..=599 => format!("TEMPORARY ERROR: {detail}"),
        _ => detail.to_string(),
    }
}

/// The failure a batch should lead with, if any lookup was a throttle,
/// a timeout, a restart, or a refused credential. A plain miss stays out
/// so a grouped cite of the DOIs that did resolve still runs.
pub fn severe_lookup_message(failures: &[String]) -> Option<String> {
    let mut severe: Vec<&String> = failures
        .iter()
        .filter(|line| is_severe_lookup(line))
        .collect();
    if severe.is_empty() {
        return None;
    }
    severe.sort_by_key(|line| failure_rank(line));
    Some(
        severe
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

pub fn failure_rank(text: &str) -> u8 {
    if text.contains("RATE LIMITED") {
        0
    } else if text.contains("TIMEOUT") {
        1
    } else if text.contains("TEMPORARY ERROR") {
        2
    } else if text.contains("ACCESS DENIED") {
        3
    } else {
        4
    }
}

fn is_severe_lookup(text: &str) -> bool {
    text.contains("RATE LIMITED")
        || text.contains("TIMEOUT")
        || text.contains("TEMPORARY ERROR")
        || text.contains("ACCESS DENIED")
        || text.starts_with("ERROR ")
}

/// What `call_tool` attaches when the tool text is an upstream failure.
/// Absent when the text is a normal answer, including a citation miss.
pub struct UpstreamFailureReport {
    pub kind: &'static str,
    pub http_status: u16,
    pub retry_after: Option<String>,
}

pub fn upstream_failure_report(text: &str) -> Option<UpstreamFailureReport> {
    let (kind, http_status, marker) = if text.contains("RATE LIMITED") {
        ("rate_limited", 429, "RATE LIMITED")
    } else if text.contains("TIMEOUT") {
        ("timeout", 504, "TIMEOUT")
    } else if text.contains("TEMPORARY ERROR") {
        ("temporary_error", 503, "TEMPORARY ERROR")
    } else {
        return None;
    };
    Some(UpstreamFailureReport {
        kind,
        http_status,
        retry_after: retry_after_on_marked_line(text, marker),
    })
}

fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(MAX_RETRY_AFTER_CHARS).collect())
}

fn retry_after_on_marked_line(text: &str, marker: &str) -> Option<String> {
    text.lines().find_map(|line| {
        if line.contains(marker) {
            extract_retry_after(line)
        } else {
            None
        }
    })
}

fn extract_retry_after(text: &str) -> Option<String> {
    const OPEN: &str = "(Retry-After: ";
    let start = text.find(OPEN)? + OPEN.len();
    let rest = &text[start..];
    let end = rest.find(')')?;
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.chars().take(MAX_RETRY_AFTER_CHARS).collect())
    }
}

fn format_error_detail(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(message) = json["message"].as_str() {
            return format!("{status}: {}", bounded(message));
        }
    }
    if body.is_empty() {
        format!("{status}")
    } else {
        format!("{status}: {}", bounded(body))
    }
}

fn bounded(text: &str) -> String {
    text.chars().take(MAX_ERROR_BODY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, retry_after: Option<&str>, body: &str) -> reqwest::Response {
        let mut builder = http::Response::builder().status(status);
        if let Some(retry_after) = retry_after {
            builder = builder.header("retry-after", retry_after);
        }
        reqwest::Response::from(builder.body(body.to_string()).unwrap())
    }

    #[tokio::test]
    async fn error_detail_keeps_a_numeric_retry_after() {
        let detail = error_detail(response(429, Some("75"), "Daily limit reached")).await;
        assert_eq!(
            detail,
            "429 Too Many Requests: Daily limit reached (Retry-After: 75)"
        );
    }

    #[tokio::test]
    async fn error_detail_keeps_an_http_date_retry_after() {
        let date = "Wed, 21 Oct 2015 07:28:00 GMT";
        let detail = error_detail(response(429, Some(date), "Daily limit reached")).await;
        assert!(detail.contains("(Retry-After: Wed, 21 Oct 2015 07:28:00 GMT)"));
    }

    #[tokio::test]
    async fn error_detail_keeps_a_malformed_retry_after() {
        let detail = error_detail(response(503, Some("not-a-delay"), "down")).await;
        assert!(detail.contains("(Retry-After: not-a-delay)"));
    }

    #[tokio::test]
    async fn error_detail_omits_an_absent_retry_after() {
        let detail = error_detail(response(429, None, "Rate limited")).await;
        assert_eq!(detail, "429 Too Many Requests: Rate limited");
        assert!(!detail.contains("Retry-After"));
    }

    #[test]
    fn upstream_report_uses_the_rate_limit_when_a_batch_also_failed_over() {
        let text = "\
[2] TEMPORARY ERROR 10.1/slow : 503 Service Unavailable (Retry-After: 2)
[1] RATE LIMITED 10.2/cap : 429 Too Many Requests: Daily limit (Retry-After: 75)
Check remaining quota";
        let report = upstream_failure_report(text).unwrap();
        assert_eq!(report.kind, "rate_limited");
        assert_eq!(report.http_status, 429);
        assert_eq!(report.retry_after.as_deref(), Some("75"));
    }

    #[test]
    fn upstream_report_is_absent_for_a_miss() {
        assert!(upstream_failure_report("INVALID 10.9/missing : HTTP 404 Not Found").is_none());
    }

    #[tokio::test]
    async fn prefixed_failure_keeps_the_operation_and_the_marker() {
        let text = prefixed_failure(
            response(503, Some("0"), "upstream restarting"),
            "Batch add failed",
        )
        .await;
        assert!(text.starts_with("Batch add failed: TEMPORARY ERROR:"));
        assert!(text.contains("upstream restarting"));
        assert!(text.contains("(Retry-After: 0)"));
        let client = prefixed_failure(
            response(400, None, "duplicate entry already present"),
            "Failed to add entry",
        )
        .await;
        assert!(client.starts_with("Failed to add entry:"));
        assert!(!client.contains("TEMPORARY ERROR"));
    }

    #[test]
    fn severe_lookup_keeps_each_throttle_and_drops_a_miss() {
        let failures = vec![
            "INVALID 10.1/missing : HTTP 404 Not Found".into(),
            "TEMPORARY ERROR 10.2/slow : 503 Service Unavailable".into(),
            "RATE LIMITED 10.3/cap : 429 Too Many Requests: Daily limit (Retry-After: 8)".into(),
        ];
        let message = severe_lookup_message(&failures).unwrap();
        assert!(message.starts_with("RATE LIMITED"));
        assert!(message.contains("TEMPORARY ERROR"));
        assert!(!message.contains("INVALID"));
    }
}
