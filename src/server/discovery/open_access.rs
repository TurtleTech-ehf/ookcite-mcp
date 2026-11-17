//! `find_open_access_pdf`: a legal open-access copy of a paper, by DOI.
//!
//! The answer comes from Unpaywall, which indexes publisher-hosted open
//! access, repositories, and preprint servers, and states a version and a
//! licence for each copy. The tool returns links. It does not download or
//! mirror a file, and it never queries a shadow library.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use super::{contact_email, doi_path, parse_doi_arg};
use crate::server::Server;

/// How many alternative copies the answer lists after the best one.
const MAX_OTHER_COPIES: usize = 4;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindOpenAccessPdfArgs {
    /// DOI of the paper: bare ("10.1371/journal.pone.0000308"), "doi:" prefixed, or a doi.org URL.
    #[schemars(
        length(min = 7, max = 512),
        regex(
            pattern = r"^\s*((https?://(dx\.)?doi\.org/|[dD][oO][iI]:\s*))?10\.[0-9]{4,9}/\S+\s*$"
        )
    )]
    pub doi: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OaCopy {
    pdf: Option<String>,
    landing: Option<String>,
    host: String,
    version: Option<String>,
    license: Option<String>,
}

impl OaCopy {
    fn from_location(location: &Value) -> Option<Self> {
        let pdf = web_url(&location["url_for_pdf"]);
        let landing =
            web_url(&location["url_for_landing_page"]).or_else(|| web_url(&location["url"]));
        if pdf.is_none() && landing.is_none() {
            return None;
        }
        let host = match location["host_type"].as_str() {
            Some("publisher") => "publisher".to_string(),
            Some("repository") => match location["repository_institution"]
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                Some(name) => format!("repository ({name})"),
                None => "repository".to_string(),
            },
            Some(other) if !other.is_empty() => other.to_string(),
            _ => "unknown host".to_string(),
        };
        Some(Self {
            pdf,
            landing,
            host,
            version: location["version"]
                .as_str()
                .map(version_label)
                .map(str::to_string),
            license: location["license"]
                .as_str()
                .map(str::trim)
                .filter(|license| !license.is_empty())
                .map(str::to_string),
        })
    }

    fn same_target(&self, other: &Self) -> bool {
        self.pdf.is_some() && self.pdf == other.pdf
            || self.pdf.is_none() && other.pdf.is_none() && self.landing == other.landing
    }

    fn detail(&self) -> String {
        let mut parts = vec![format!("host: {}", self.host)];
        if let Some(version) = &self.version {
            parts.push(format!("version: {version}"));
        }
        parts.push(format!(
            "licence: {}",
            self.license.as_deref().unwrap_or("not stated")
        ));
        parts.join(" | ")
    }
}

/// A link a reader can follow. Anything that is not http or https, or that
/// carries credentials or control characters, is dropped.
fn web_url(value: &Value) -> Option<String> {
    let raw = value.as_str()?.trim();
    if raw.is_empty() || raw.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return None;
    }
    let url = reqwest::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return None;
    }
    Some(url.to_string())
}

fn version_label(raw: &str) -> &str {
    match raw {
        "publishedVersion" => "published version",
        "acceptedVersion" => "accepted manuscript",
        "submittedVersion" => "submitted manuscript (preprint)",
        other => other,
    }
}

pub(super) fn render_open_access(doi: &str, body: &Value) -> String {
    let title = body["title"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let journal = body["journal_name"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let year = body["year"].as_i64();
    let status = body["oa_status"].as_str().unwrap_or("unknown");

    let mut copies: Vec<OaCopy> = Vec::new();
    let listed = body["oa_locations"].as_array().cloned().unwrap_or_default();
    for location in std::iter::once(&body["best_oa_location"]).chain(listed.iter()) {
        let Some(copy) = OaCopy::from_location(location) else {
            continue;
        };
        if !copies.iter().any(|seen| seen.same_target(&copy)) {
            copies.push(copy);
        }
    }

    let mut out = Vec::new();
    let is_oa = body["is_oa"].as_bool().unwrap_or(false);
    if !is_oa || copies.is_empty() {
        out.push(format!(
            "NOT OPEN ACCESS doi:{doi}{}",
            if is_oa {
                " (Unpaywall marks it open but lists no usable link)"
            } else {
                ""
            }
        ));
        push_work_lines(&mut out, title, journal, year);
        out.push(format!("OA status: {status}"));
        out.push(
            "No free legal copy is recorded. A repository may add an accepted manuscript later; \
             the author or a library can supply one."
                .into(),
        );
        out.push(source_line());
        return out.join("\n");
    }

    out.push(format!("OPEN ACCESS doi:{doi}"));
    push_work_lines(&mut out, title, journal, year);
    out.push(format!("OA status: {status}"));
    let best = &copies[0];
    match (&best.pdf, &best.landing) {
        (Some(pdf), landing) => {
            out.push(format!("Best copy (PDF): {pdf}"));
            if let Some(landing) = landing {
                out.push(format!("Landing page: {landing}"));
            }
        }
        (None, Some(landing)) => {
            out.push(format!(
                "Best copy (landing page, no direct PDF link): {landing}"
            ));
        }
        (None, None) => unreachable!("a copy always has a link"),
    }
    out.push(format!("  {}", best.detail()));
    let others = &copies[1..];
    if !others.is_empty() {
        out.push(format!("Other legal copies ({}):", others.len()));
        for copy in others.iter().take(MAX_OTHER_COPIES) {
            let link = copy
                .pdf
                .as_ref()
                .map(|pdf| format!("PDF {pdf}"))
                .or_else(|| copy.landing.as_ref().map(|page| format!("page {page}")))
                .unwrap_or_default();
            out.push(format!("  - {link} ({})", copy.detail()));
        }
        if others.len() > MAX_OTHER_COPIES {
            out.push(format!(
                "  ... and {} more",
                others.len() - MAX_OTHER_COPIES
            ));
        }
    }
    out.push(source_line());
    out.join("\n")
}

fn push_work_lines(
    out: &mut Vec<String>,
    title: Option<&str>,
    journal: Option<&str>,
    year: Option<i64>,
) {
    if let Some(title) = title {
        out.push(format!("Title: {title}"));
    }
    match (journal, year) {
        (Some(journal), Some(year)) => out.push(format!("Venue: {journal} ({year})")),
        (Some(journal), None) => out.push(format!("Venue: {journal}")),
        (None, Some(year)) => out.push(format!("Year: {year}")),
        (None, None) => {}
    }
}

fn source_line() -> String {
    "Source: Unpaywall. Links only: no file was downloaded, and no shadow library was consulted."
        .into()
}

#[tool_router(router = open_access_tool_router, vis = "pub(super)")]
impl Server {
    #[tool(
        name = "find_open_access_pdf",
        description = "Find a legal open-access copy of a paper by DOI. Returns the best free PDF or landing-page link with its host (publisher or repository), version (published, accepted, or submitted), and licence, plus other legal copies. Call this when the user wants to read a paper they cannot get through a paywall. Only copies that Unpaywall records as open access are returned, and nothing is downloaded: the result is a link. NOT OPEN ACCESS means no free legal copy is recorded, which is a real answer and not a failure. A 429 or 402 from the provider is reported with its Retry-After and is not a miss. Use validate_doi first when the DOI itself is in doubt.",
        annotations(
            title = "Find open-access PDF",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn find_open_access_pdf(
        &self,
        Parameters(args): Parameters<FindOpenAccessPdfArgs>,
    ) -> String {
        let doi = match parse_doi_arg(&args.doi) {
            Ok(doi) => doi,
            Err(message) => return message,
        };
        let Some(bases) = self.discovery_bases() else {
            return "ERROR Unpaywall : provider access is disabled".into();
        };
        let url = format!(
            "{}/v2/{}",
            bases.unpaywall.trim_end_matches('/'),
            doi_path(&doi)
        );
        match self
            .discovery_json("Unpaywall", &url, &[("email", contact_email())])
            .await
        {
            Ok(Some(body)) if body["error"].as_bool() == Some(true) => {
                format!("NOT FOUND doi:{doi}\nUnpaywall has no record of this DOI.")
            }
            Ok(Some(body)) => render_open_access(&doi, &body),
            Ok(None) => format!(
                "NOT FOUND doi:{doi}\nUnpaywall has no record of this DOI. Check it with validate_doi."
            ),
            Err(message) => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, ContentBlock};
    use rmcp::ServerHandler;
    use serde_json::json;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const DOI: &str = "10.5555/demo.oa.2024";

    fn server(mock: &MockServer) -> Server {
        Server::new_with_base("http://127.0.0.1:9".into()).with_discovery(mock.uri())
    }

    fn args(doi: &str) -> Parameters<FindOpenAccessPdfArgs> {
        Parameters(FindOpenAccessPdfArgs { doi: doi.into() })
    }

    fn unpaywall_gold() -> Value {
        json!({
            "doi": DOI,
            "title": "A Mock Study of Open Access Links",
            "journal_name": "Journal of Mock Results",
            "year": 2024,
            "is_oa": true,
            "oa_status": "gold",
            "best_oa_location": {
                "url": "https://journals.example.org/article/demo.oa.2024",
                "url_for_pdf": "https://journals.example.org/article/demo.oa.2024.pdf",
                "url_for_landing_page": "https://journals.example.org/article/demo.oa.2024",
                "host_type": "publisher",
                "version": "publishedVersion",
                "license": "cc-by"
            },
            "oa_locations": [
                {
                    "url_for_pdf": "https://journals.example.org/article/demo.oa.2024.pdf",
                    "url_for_landing_page": "https://journals.example.org/article/demo.oa.2024",
                    "host_type": "publisher",
                    "version": "publishedVersion",
                    "license": "cc-by"
                },
                {
                    "url_for_pdf": null,
                    "url_for_landing_page": "https://repo.example.edu/handle/demo/2024",
                    "host_type": "repository",
                    "repository_institution": "Example University",
                    "version": "publishedVersion",
                    "license": null
                },
                {
                    "url_for_pdf": "javascript:alert(1)",
                    "url_for_landing_page": "ftp://example.org/paper.pdf",
                    "host_type": "repository",
                    "version": "acceptedVersion"
                }
            ]
        })
    }

    #[tokio::test]
    async fn returns_the_best_pdf_other_copies_and_no_download_notice() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/{DOI}")))
            .and(|request: &Request| {
                request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "email" && value.contains('@'))
            })
            .and(header_exists("user-agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(unpaywall_gold()))
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock).find_open_access_pdf(args(DOI)).await;

        assert!(
            text.starts_with("OPEN ACCESS doi:10.5555/demo.oa.2024"),
            "{text}"
        );
        assert!(
            text.contains("Title: A Mock Study of Open Access Links"),
            "{text}"
        );
        assert!(
            text.contains("Venue: Journal of Mock Results (2024)"),
            "{text}"
        );
        assert!(text.contains("OA status: gold"), "{text}");
        assert!(
            text.contains("Best copy (PDF): https://journals.example.org/article/demo.oa.2024.pdf"),
            "{text}"
        );
        assert!(
            text.contains("host: publisher | version: published version | licence: cc-by"),
            "{text}"
        );
        assert!(text.contains("Other legal copies (1):"), "{text}");
        assert!(text.contains("repository (Example University)"), "{text}");
        assert!(text.contains("licence: not stated"), "{text}");
        assert!(text.contains("no file was downloaded"), "{text}");
        assert!(
            !text.contains("javascript:") && !text.contains("ftp://"),
            "{text}"
        );
        assert!(
            crate::http_error::upstream_failure_report(&text).is_none(),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_landing_page_only_copy_says_there_is_no_direct_pdf() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/10.1000/green.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "is_oa": true,
                "oa_status": "green",
                "best_oa_location": {
                    "url": "https://repo.example.edu/handle/1",
                    "url_for_pdf": null,
                    "url_for_landing_page": "https://repo.example.edu/handle/1",
                    "host_type": "repository",
                    "version": "acceptedVersion",
                    "license": "cc-by-nc"
                },
                "oa_locations": []
            })))
            .mount(&mock)
            .await;

        let text = server(&mock)
            .find_open_access_pdf(args("10.1000/green.1"))
            .await;

        assert!(
            text.contains(
                "Best copy (landing page, no direct PDF link): https://repo.example.edu/handle/1"
            ),
            "{text}"
        );
        assert!(text.contains("version: accepted manuscript"), "{text}");
    }

    #[tokio::test]
    async fn a_closed_paper_is_a_not_open_access_answer_not_an_error() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/10.1038/187493a0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "title": "Stimulated Optical Radiation in Ruby",
                "journal_name": "Nature",
                "year": 1960,
                "is_oa": false,
                "oa_status": "closed",
                "best_oa_location": null,
                "oa_locations": []
            })))
            .mount(&mock)
            .await;

        let text = server(&mock)
            .find_open_access_pdf(args("10.1038/187493a0"))
            .await;

        assert!(
            text.starts_with("NOT OPEN ACCESS doi:10.1038/187493a0"),
            "{text}"
        );
        assert!(text.contains("OA status: closed"), "{text}");
        assert!(!text.contains("Best copy"), "{text}");
        assert!(
            crate::http_error::upstream_failure_report(&text).is_none(),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_unknown_doi_is_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": true,
                "message": "'10.1000/nope' isn't in Unpaywall"
            })))
            .mount(&mock)
            .await;

        let text = server(&mock)
            .find_open_access_pdf(args("10.1000/nope"))
            .await;

        assert!(text.starts_with("NOT FOUND doi:10.1000/nope"), "{text}");
        assert!(
            crate::http_error::upstream_failure_report(&text).is_none(),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_doi_url_and_a_doi_prefix_reach_the_same_provider_path() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/{DOI}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(unpaywall_gold()))
            .expect(2)
            .mount(&mock)
            .await;
        let s = server(&mock);

        let by_url = s
            .find_open_access_pdf(args(&format!("https://doi.org/{}", DOI.to_uppercase())))
            .await;
        let by_prefix = s.find_open_access_pdf(args(&format!("doi:{DOI}"))).await;

        assert!(by_url.starts_with("OPEN ACCESS"), "{by_url}");
        assert!(by_prefix.starts_with("OPEN ACCESS"), "{by_prefix}");
    }

    #[tokio::test]
    async fn a_malformed_doi_is_refused_before_any_request() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let text = server(&mock).find_open_access_pdf(args("not a doi")).await;

        assert!(text.starts_with("CLIENT ERROR:"), "{text}");
        let report = crate::http_error::upstream_failure_report(&text).unwrap();
        assert_eq!(report.kind, "client_error");
    }

    #[tokio::test]
    async fn a_429_keeps_its_retry_after_and_is_not_a_miss() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "120")
                    .set_body_json(json!({"message": "Too many requests"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock).find_open_access_pdf(args(DOI)).await;

        assert!(
            text.starts_with("RATE LIMITED Unpaywall : 429 Too Many Requests"),
            "{text}"
        );
        assert!(text.contains("(Retry-After: 120)"), "{text}");
        assert!(
            !text.contains("NOT OPEN ACCESS") && !text.contains("NOT FOUND"),
            "{text}"
        );
        let report = crate::http_error::upstream_failure_report(&text).unwrap();
        assert_eq!(report.http_status, Some(429));
        assert_eq!(report.retry_after.as_deref(), Some("120"));
        assert!(
            include_str!("../../../README.md").contains(&text),
            "README throttle example is stale; it should contain:\n{text}"
        );
    }

    #[tokio::test]
    async fn a_402_keeps_its_retry_after_and_is_not_a_miss() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("retry-after", "86400")
                    .set_body_json(json!({"message": "API credits exhausted"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock).find_open_access_pdf(args(DOI)).await;

        assert!(
            text.starts_with("PAYMENT REQUIRED Unpaywall : 402 Payment Required"),
            "{text}"
        );
        assert!(
            text.contains("answered 402, which its public API does not document"),
            "{text}"
        );
        assert!(!text.contains("wants payment"), "{text}");
        let mut result = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        crate::server::promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["kind"], "payment_required");
        assert_eq!(structured["http_status"], 402);
        assert_eq!(structured["retry_after"], "86400");
    }

    #[tokio::test]
    async fn a_restart_is_retried_then_reported_as_temporary() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .expect(3)
            .mount(&mock)
            .await;

        let text = server(&mock).find_open_access_pdf(args(DOI)).await;

        assert!(
            text.starts_with("TEMPORARY ERROR Unpaywall : 503"),
            "{text}"
        );
        assert!(text.contains("(Retry-After: 0)"), "{text}");
    }

    #[test]
    fn only_web_links_without_credentials_are_kept() {
        assert!(web_url(&json!("https://example.org/a.pdf")).is_some());
        assert!(web_url(&json!("http://example.org/a.pdf")).is_some());
        assert!(web_url(&json!("javascript:alert(1)")).is_none());
        assert!(web_url(&json!("ftp://example.org/a.pdf")).is_none());
        assert!(web_url(&json!("https://user:pw@example.org/a.pdf")).is_none());
        assert!(web_url(&json!("https://example.org/a b.pdf")).is_none());
        assert!(web_url(&json!(null)).is_none());
    }

    #[test]
    fn the_readme_transcript_is_the_tool_output() {
        let readme = include_str!("../../../README.md");
        let output = render_open_access("10.5555/demo.oa.2024", &unpaywall_gold());
        assert!(
            readme.contains(&output),
            "README transcript for find_open_access_pdf is stale; it should contain:\n{output}"
        );
    }

    #[test]
    fn the_input_schema_is_closed_and_requires_a_doi() {
        let tool = Server::new_with_base("http://127.0.0.1:9".into())
            .get_tool("find_open_access_pdf")
            .expect("tool is registered");
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["doi"]));
        let doi = &schema["properties"]["doi"];
        assert_eq!(doi["type"], "string");
        assert_eq!(doi["minLength"], 7);
        assert_eq!(doi["maxLength"], 512);
        let pattern = doi["pattern"].as_str().expect("doi has a pattern");
        assert!(pattern.contains("10\\.[0-9]{4,9}/"), "{pattern}");
        assert_eq!(
            tool.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
    }
}
