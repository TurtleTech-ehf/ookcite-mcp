//! HTTP error formatting and classification for tool responses.

use crate::constants::rate_limit_hint;

const MAX_ERROR_BODY_CHARS: usize = 120;
/// Enough for four bytes per kept character plus a JSON wrapper.
const MAX_ERROR_BODY_READ_BYTES: usize = 4096;
const MAX_RETRY_AFTER_CHARS: usize = 128;

/// Extract a useful error message from a failed HTTP response.
///
/// The status, a bounded body excerpt, and `Retry-After` when the server
/// sent one all stay in the text. A later tool result can tell a throttle
/// from a missing citation.
pub async fn error_detail(resp: reqwest::Response) -> String {
    let status = resp.status();
    let retry_after = retry_after_header(resp.headers());
    let body = read_error_body(resp).await;
    let mut detail = format_error_detail(status, &body);
    if let Some(retry_after) = retry_after {
        detail.push_str(" (Retry-After: ");
        detail.push_str(&retry_after);
        detail.push(')');
    }
    detail
}

/// The start of an error body. Only the first [`MAX_ERROR_BODY_CHARS`] characters
/// reach the tool text, so a hostile or broken upstream that streams without
/// end is not read past a few kilobytes.
async fn read_error_body(mut resp: reqwest::Response) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < MAX_ERROR_BODY_READ_BYTES {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_ERROR_BODY_READ_BYTES - buf.len();
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

pub async fn classify_lookup_doi_failure(resp: reqwest::Response, doi: &str) -> String {
    let status = resp.status();
    let detail = error_detail(resp).await;
    if status.as_u16() == 429 {
        format!("RATE LIMITED {doi} : {detail}\n{}", rate_limit_hint())
    } else if status.as_u16() == 402 {
        format!("PAYMENT REQUIRED {doi} : {detail}")
    } else if matches!(status.as_u16(), 401 | 403) {
        format!("ACCESS DENIED {doi} : {detail}")
    } else if matches!(status.as_u16(), 408 | 504) {
        format!("TIMEOUT {doi} : {detail}")
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
    matches!(status.as_u16(), 402 | 408 | 429 | 500..=599)
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
        402 => format!("PAYMENT REQUIRED: {detail}"),
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
    } else if text.contains("PAYMENT REQUIRED") || text.contains("ACCESS DENIED") {
        3
    } else {
        4
    }
}

fn is_severe_lookup(text: &str) -> bool {
    text.contains("RATE LIMITED")
        || text.contains("TIMEOUT")
        || text.contains("TEMPORARY ERROR")
        || text.contains("UNREACHABLE")
        || text.contains("PAYMENT REQUIRED")
        || text.contains("ACCESS DENIED")
        || text.starts_with("ERROR ")
}

/// What `call_tool` attaches when the tool text is an upstream failure.
/// Absent when the text is a normal answer, including a citation miss.
pub struct UpstreamFailureReport {
    pub kind: &'static str,
    pub http_status: Option<u16>,
    pub retry_after: Option<String>,
}

pub fn upstream_failure_report(text: &str) -> Option<UpstreamFailureReport> {
    let (kind, http_status, marker) = if text.contains("RATE LIMITED") {
        ("rate_limited", Some(429), Some("RATE LIMITED"))
    } else if text.contains("PAYMENT REQUIRED") {
        ("payment_required", Some(402), Some("PAYMENT REQUIRED"))
    } else if text.contains("UNREACHABLE") {
        ("unreachable", None, None)
    } else if text.contains("TIMEOUT") {
        ("timeout", Some(timeout_status(text)), Some("TIMEOUT"))
    } else if text.contains("TEMPORARY ERROR") {
        ("temporary_error", Some(503), Some("TEMPORARY ERROR"))
    } else if line_has_prefix(text, "ACCESS DENIED") {
        (
            "access_denied",
            Some(status_on_marked_line(text, "ACCESS DENIED").unwrap_or(403)),
            Some("ACCESS DENIED"),
        )
    } else if line_has_prefix(text, "AUTH REQUIRED") {
        ("auth_required", Some(401), None)
    } else if line_has_prefix(text, "CLIENT ERROR") {
        (
            "client_error",
            Some(status_on_marked_line(text, "CLIENT ERROR").unwrap_or(400)),
            None,
        )
    } else if line_has_prefix(text, "UNREGISTERED") {
        ("unregistered", None, None)
    } else if line_has_prefix(text, "ERROR") {
        ("error", status_on_marked_line(text, "ERROR"), None)
    } else if line_has_prefix(text, "Failed") {
        ("failed", None, None)
    } else if line_has_prefix(text, "REFUSED") {
        ("refused", None, None)
    } else {
        return None;
    };
    Some(UpstreamFailureReport {
        kind,
        http_status,
        retry_after: marker.and_then(|marker| retry_after_on_marked_line(text, marker)),
    })
}

/// A failure that stops a check: a throttle, a payment wall, a timeout, or a
/// restart. Anything else the provider said, such as a 403 or a 422, only
/// leaves the entry unchecked.
pub fn is_stopping_failure(text: &str) -> bool {
    upstream_failure_report(text).is_some_and(|report| {
        matches!(
            report.kind,
            "rate_limited" | "payment_required" | "timeout" | "temporary_error" | "unreachable"
        )
    })
}

/// The HTTP status a provider failure line names. The text is
/// `<MARKER> <source> : <status> <reason>: <detail>`, and the status is read
/// only after the first ` : `, so digits in a DOI or a host never count.
/// `None` when the line carries no status, as for a body that was too large.
fn status_on_marked_line(text: &str, marker: &str) -> Option<u16> {
    text.lines().find_map(|line| {
        let line = line.trim_start();
        let line = match line.strip_prefix('[') {
            Some(rest) => rest
                .split_once(']')
                .map_or(line, |(_, after)| after.trim_start()),
            None => line,
        };
        let rest = line.strip_prefix(marker)?;
        let (_, detail) = rest.split_once(" : ")?;
        let digits: String = detail.chars().take_while(char::is_ascii_digit).collect();
        let ends_cleanly = detail[digits.len()..]
            .chars()
            .next()
            .is_none_or(|next| next == ' ' || next == ':');
        if digits.len() != 3 || !ends_cleanly {
            return None;
        }
        digits
            .parse()
            .ok()
            .filter(|status| (100..=599).contains(status))
    })
}

fn timeout_status(text: &str) -> u16 {
    if text
        .lines()
        .filter_map(|line| line.split_once("TIMEOUT").map(|(_, detail)| detail))
        .filter_map(|detail| {
            detail
                .strip_prefix(": ")
                .or_else(|| detail.split_once(" : ").map(|(_, status)| status))
        })
        .any(|status| status.starts_with("408 Request Timeout:"))
    {
        408
    } else {
        504
    }
}

/// A verdict line may be prefixed with `[n]`. Bibliographic misses stay unmarked.
fn line_has_prefix(text: &str, prefix: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        let line = match line.strip_prefix('[') {
            Some(rest) => match rest.split_once(']') {
                Some((index, after)) if index.chars().all(|ch| ch.is_ascii_digit()) => {
                    after.trim_start()
                }
                _ => line,
            },
            None => line,
        };
        line.starts_with(prefix)
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

/// A 404 that is not a named missing library means the save inbox route
/// is not on this server.
///
/// Callers answer with [`crate::ridley_library::SAVE_INBOX_UNAVAILABLE`].
/// `promote_upstream_failure` is the only place that sets `is_error` and
/// the structured 404 for that text.
pub fn save_inbox_unavailable(status: reqwest::StatusCode, error_code: Option<&str>) -> bool {
    status == reqwest::StatusCode::NOT_FOUND
        && !matches!(
            error_code,
            Some("library_not_found" | "ridley_library_not_found")
        )
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

    #[test]
    fn save_inbox_unavailable_is_a_404_that_is_not_a_named_library() {
        assert!(save_inbox_unavailable(
            reqwest::StatusCode::NOT_FOUND,
            Some("not_found")
        ));
        assert!(save_inbox_unavailable(reqwest::StatusCode::NOT_FOUND, None));
        assert!(!save_inbox_unavailable(
            reqwest::StatusCode::NOT_FOUND,
            Some("library_not_found")
        ));
        assert!(!save_inbox_unavailable(
            reqwest::StatusCode::NOT_FOUND,
            Some("ridley_library_not_found")
        ));
        assert!(!save_inbox_unavailable(
            reqwest::StatusCode::FORBIDDEN,
            None
        ));
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
        assert_eq!(report.http_status, Some(429));
        assert_eq!(report.retry_after.as_deref(), Some("75"));
    }

    #[test]
    fn upstream_report_is_absent_for_a_miss() {
        assert!(upstream_failure_report("INVALID 10.9/missing : HTTP 404 Not Found").is_none());
        assert!(upstream_failure_report("Not found").is_none());
        assert!(upstream_failure_report("Could not resolve").is_none());
        assert!(upstream_failure_report("No matches found").is_none());
        assert!(upstream_failure_report("VALID 10.1/ok : A paper").is_none());
        assert!(upstream_failure_report("MISMATCH 10.1/ok : A paper").is_none());
        assert!(upstream_failure_report("INCOMPLETE 10.1/ok : A paper").is_none());
    }

    #[test]
    fn upstream_report_marks_a_transport_error_and_an_unregistered_doi() {
        let transport = upstream_failure_report("ERROR 10.1/x : error sending request").unwrap();
        assert_eq!(transport.kind, "error");
        assert_eq!(transport.http_status, None);
        let unregistered = upstream_failure_report(
            "UNREGISTERED 10.2307/4615733: the DOI is not registered at doi.org",
        )
        .unwrap();
        assert_eq!(unregistered.kind, "unregistered");
        assert_eq!(unregistered.http_status, None);
        let numbered = upstream_failure_report("[2] ERROR 10.1/x : error sending request").unwrap();
        assert_eq!(numbered.kind, "error");
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

#[cfg(test)]
mod timeout_status_regression_tests {
    #[test]
    fn a_gateway_error_body_cannot_replace_its_status() {
        let report = super::upstream_failure_report(
            "TIMEOUT 10.1234/example : 504 Gateway Timeout: upstream mentioned 408 Request Timeout",
        )
        .unwrap();
        assert_eq!(report.http_status, Some(504));
    }

    #[test]
    fn a_timeout_without_a_doi_keeps_its_request_status() {
        let report =
            super::upstream_failure_report("TIMEOUT: 408 Request Timeout: request expired")
                .unwrap();
        assert_eq!(report.http_status, Some(408));
    }
}

#[cfg(test)]
mod payment_required_tests {
    use super::*;

    fn response(status: u16, retry_after: Option<&str>, body: &str) -> reqwest::Response {
        let mut builder = http::Response::builder().status(status);
        if let Some(retry_after) = retry_after {
            builder = builder.header("retry-after", retry_after);
        }
        reqwest::Response::from(builder.body(body.to_string()).unwrap())
    }

    #[tokio::test]
    async fn a_402_is_named_and_keeps_its_retry_after() {
        let text = failure_text(
            response(402, Some("3600"), r#"{"message":"plan required"}"#),
            "Lookup failed",
        )
        .await;
        assert_eq!(
            text,
            "PAYMENT REQUIRED: 402 Payment Required: plan required (Retry-After: 3600)"
        );
        let report = upstream_failure_report(&text).unwrap();
        assert_eq!(report.kind, "payment_required");
        assert_eq!(report.http_status, Some(402));
        assert_eq!(report.retry_after.as_deref(), Some("3600"));
    }

    #[tokio::test]
    async fn a_doi_lookup_402_is_not_a_client_error() {
        let text = classify_lookup_doi_failure(response(402, None, "quota"), "10.1/x").await;
        assert!(text.starts_with("PAYMENT REQUIRED 10.1/x : 402"), "{text}");
        assert_eq!(
            upstream_failure_report(&text).unwrap().kind,
            "payment_required"
        );
        assert!(severe_lookup_message(&[text]).is_some());
    }

    #[test]
    fn a_402_is_found_anywhere_in_the_text_like_a_429() {
        let text = "[k] UNCHECKED doi:10.1/x: PAYMENT REQUIRED api.crossref.org : 402 Payment Required (Retry-After: 60)";
        let report = upstream_failure_report(text).unwrap();
        assert_eq!(report.kind, "payment_required");
        assert_eq!(report.http_status, Some(402));
        assert_eq!(report.retry_after.as_deref(), Some("60"));
        let throttle = text.replace("PAYMENT REQUIRED", "RATE LIMITED");
        assert_eq!(
            upstream_failure_report(&throttle).unwrap().kind,
            "rate_limited"
        );
    }

    #[tokio::test]
    async fn an_endless_error_body_is_not_read_to_its_end() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let head = "HTTP/1.1 502 Bad Gateway\r\ntransfer-encoding: chunked\r\n\r\n";
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            let chunk = format!("400\r\n{}\r\n", "x".repeat(1024));
            while socket.write_all(chunk.as_bytes()).await.is_ok() {}
        });
        let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();

        let detail = tokio::time::timeout(std::time::Duration::from_secs(10), error_detail(resp))
            .await
            .expect("error_detail kept reading an endless body");

        assert_eq!(
            detail.chars().filter(|c| *c == 'x').count(),
            MAX_ERROR_BODY_CHARS
        );
    }

    #[test]
    fn a_refusal_reports_the_status_the_provider_sent() {
        let table = [
            (
                "ACCESS DENIED doi.org for doi:10.1/x : 401 Unauthorized: no",
                "access_denied",
                401,
            ),
            (
                "ACCESS DENIED doi.org for doi:10.1/x : 403 Forbidden: no",
                "access_denied",
                403,
            ),
            (
                "ERROR doi.org for doi:10.403/x : 400 Bad Request: no",
                "error",
                400,
            ),
            (
                "    ERROR doi.org for doi:10.1/x : 409 Conflict",
                "error",
                409,
            ),
            (
                "ERROR doi.org for doi:10.1/x : 422 Unprocessable Entity: no",
                "error",
                422,
            ),
            (
                "CLIENT ERROR 10.1/x : 418 I'm a teapot",
                "client_error",
                418,
            ),
        ];
        for (text, kind, status) in table {
            let report = upstream_failure_report(text).unwrap();
            assert_eq!(report.kind, kind, "{text}");
            assert_eq!(report.http_status, Some(status), "{text}");
        }
        let no_status =
            upstream_failure_report("ERROR corpus : provider response exceeded 9 bytes");
        assert_eq!(no_status.unwrap().http_status, None);
    }

    #[test]
    fn an_unreachable_provider_is_an_error_with_no_http_status() {
        let text =
            "[k] UPSTREAM doi:10.1/x\n    UNREACHABLE 10.1/x : timed out reading api.crossref.org";
        let report = upstream_failure_report(text).unwrap();
        assert_eq!(report.kind, "unreachable");
        assert_eq!(report.http_status, None);
        assert!(is_stopping_failure(text));
        assert!(!is_stopping_failure(
            "ACCESS DENIED h for doi:10.1/x : 403 Forbidden"
        ));
        assert!(is_severe_lookup(
            "UNREACHABLE 10.1/x : could not connect to doi.org"
        ));
    }

    #[test]
    fn a_numbered_batch_line_still_reports_a_402() {
        let report =
            upstream_failure_report("[3] PAYMENT REQUIRED 10.1/x : 402 Payment Required").unwrap();
        assert_eq!(report.http_status, Some(402));
    }
}
