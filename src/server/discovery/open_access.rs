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

fn degraded_source_line() -> String {
    "Source: Crossref or DataCite license record only (DEGRADED; set OOKCITE_MCP_CONTACT_EMAIL to \
     query Unpaywall for repository and preprint copies). Links only: no file was downloaded."
        .into()
}

fn degraded_header(doi: &str, has_oa: bool) -> String {
    if has_oa {
        format!("OPEN ACCESS doi:{doi} [DEGRADED]")
    } else {
        format!("NO LEGAL COPY RECORDED doi:{doi} [DEGRADED]")
    }
}

/// Map a bare SPDX-style Creative Commons rights identifier (as
/// DataCite records in `rightsIdentifier`) to its canonical
/// creativecommons.org URL. Any other identifier is returned verbatim.
fn canonicalise_cc_identifier(identifier: &str) -> String {
    let trimmed = identifier.trim();
    let upper = trimmed.to_ascii_uppercase();
    let mapped = match upper.as_str() {
        "CC0-1.0" | "CC0" => Some("https://creativecommons.org/publicdomain/zero/1.0/"),
        "CC-BY-4.0" => Some("https://creativecommons.org/licenses/by/4.0/"),
        "CC-BY-SA-4.0" => Some("https://creativecommons.org/licenses/by-sa/4.0/"),
        "CC-BY-NC-4.0" => Some("https://creativecommons.org/licenses/by-nc/4.0/"),
        "CC-BY-ND-4.0" => Some("https://creativecommons.org/licenses/by-nd/4.0/"),
        "CC-BY-NC-SA-4.0" => Some("https://creativecommons.org/licenses/by-nc-sa/4.0/"),
        "CC-BY-NC-ND-4.0" => Some("https://creativecommons.org/licenses/by-nc-nd/4.0/"),
        _ => None,
    };
    mapped
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string())
}

/// Render Crossref-only or DataCite-only open-access facts: Crossref's
/// `license[]` with the landing URL, or DataCite's `rightsList` (both
/// `rightsUri` and `rightsIdentifier`) with the DataCite landing URL,
/// marked DEGRADED so the caller knows the Unpaywall repository /
/// preprint sweep did not run.
pub(super) fn render_license_only(doi: &str, record: &Value) -> String {
    let crossref = record.get("crossref").filter(|value| value.is_object());
    let datacite = record.get("datacite").filter(|value| value.is_object());
    let title = crossref
        .and_then(|m| m.get("title"))
        .and_then(|value| value.as_array())
        .and_then(|array| array.first())
        .and_then(|value| value.as_str())
        .or_else(|| {
            datacite
                .and_then(|m| m.get("titles"))
                .and_then(|value| value.as_array())
                .and_then(|array| array.first())
                .and_then(|value| value.get("title"))
                .and_then(|value| value.as_str())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let journal = crossref
        .and_then(|m| m.get("container-title"))
        .and_then(|value| value.as_array())
        .and_then(|array| array.first())
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let year = crossref
        .and_then(|m| {
            m.get("issued")
                .or_else(|| m.get("published-print"))
                .or_else(|| m.get("published"))
        })
        .and_then(|value| value.get("date-parts"))
        .and_then(|value| value.as_array())
        .and_then(|array| array.first())
        .and_then(|value| value.as_array())
        .and_then(|parts| parts.first())
        .and_then(|value| value.as_i64())
        .or_else(|| {
            datacite
                .and_then(|m| m.get("publicationYear"))
                .and_then(|value| value.as_i64())
        });
    let mut licenses: Vec<String> = crossref
        .and_then(|m| m.get("license"))
        .and_then(|value| value.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.get("URL").and_then(|value| value.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // DataCite records the licence as a `rightsList[].rightsUri` or
    // `rightsIdentifier` under each item. A DataCite-only work (a
    // dataset, a Zenodo upload, an arXiv-datacite registration) has no
    // Crossref `license[]` to read, so the old path called it "not
    // stated" and closed with "NO LEGAL COPY RECORDED" even when the
    // record carried `https://creativecommons.org/licenses/by/4.0/`.
    if let Some(rights_list) = datacite
        .and_then(|m| m.get("rightsList"))
        .and_then(|value| value.as_array())
    {
        for item in rights_list {
            let url = item
                .get("rightsUri")
                .or_else(|| item.get("rightsURI"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if let Some(url) = url {
                licenses.push(url.to_string());
                continue;
            }
            // SPDX ids like "CC-BY-4.0" land in `rightsIdentifier` with
            // no URL. Map the common CC ids to their canonical URL so
            // the open-access detection below still triggers; otherwise
            // print the identifier verbatim.
            if let Some(id) = item
                .get("rightsIdentifier")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                licenses.push(canonicalise_cc_identifier(id));
            }
        }
    }
    let landing = crossref
        .and_then(|m| {
            m.pointer("/resource/primary/URL")
                .and_then(|value| value.as_str())
                .or_else(|| m.get("URL").and_then(|value| value.as_str()))
        })
        .or_else(|| datacite.and_then(|m| m.get("url").and_then(|value| value.as_str())))
        .map(str::to_string);
    let is_open = licenses.iter().any(|url| {
        let lower = url.to_ascii_lowercase();
        lower.contains("creativecommons.org/licenses")
            || lower.contains("creativecommons.org/publicdomain")
    });

    let mut out = vec![degraded_header(doi, is_open && landing.is_some())];
    push_work_lines(&mut out, title, journal, year);
    if licenses.is_empty() {
        out.push("Licence: not stated in the publisher record.".into());
    } else {
        out.push("Licences:".into());
        for license in licenses {
            out.push(format!("  - {license}"));
        }
    }
    match (is_open, landing.as_ref()) {
        (true, Some(landing)) => out.push(format!(
            "Best copy (landing page, no PDF scan without Unpaywall): {landing}"
        )),
        (false, Some(landing)) => out.push(format!(
            "Publisher landing page (open status not confirmed from licence alone): {landing}"
        )),
        (_, None) => out.push("No landing URL recorded on the publisher record.".into()),
    }
    out.push(degraded_source_line());
    out.join("\n")
}

#[tool_router(router = open_access_tool_router, vis = "pub(super)")]
impl Server {
    #[tool(
        name = "find_open_access_pdf",
        description = "Find a legal open-access copy of a paper by DOI. Returns the best free PDF or landing-page link with its host (publisher or repository), version (published, accepted, or submitted), and licence, plus other legal copies. Call this when the user wants to read a paper they cannot get through a paywall. The Unpaywall repository-and-preprint sweep needs a contact address, so set OOKCITE_MCP_CONTACT_EMAIL to the operator's own email for the full answer. When the email is unset the call does not fail: the server degrades to the publisher record (Crossref `license[]` + landing URL, or DataCite `rightsList` + URL) and returns an answer marked [DEGRADED], which still names the licence a publisher recorded and the landing page. Only copies that are legally free are returned, and nothing is downloaded: the result is a link. NOT OPEN ACCESS means no free legal copy is recorded, which is a real answer and not a failure. A 429 or 402 from the provider is reported with its Retry-After and is not a miss. Use validate_doi first when the DOI itself is in doubt.",
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
        let Some(email) = contact_email() else {
            // Without an operator address Unpaywall refuses the request, so
            // the old path returned an error and nothing else. Crossref
            // records `license[]` and a landing URL on the work itself and
            // its polite pool has no email requirement, so the free legal
            // copy a publisher recorded can still be shown. The answer is
            // marked DEGRADED so an agent knows the Unpaywall sweep of
            // repository and preprint copies did not run.
            return self.degraded_open_access(&doi).await;
        };
        let url = format!(
            "{}/v2/{}",
            bases.unpaywall.trim_end_matches('/'),
            doi_path(&doi)
        );
        match self
            .discovery_json("Unpaywall", &url, &[("email", email)])
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

    /// Open-access answer from the publisher record alone, for sessions
    /// that have not set a contact email. Reads Crossref `license[]` and
    /// the landing URL (or the DataCite rights list) and marks the answer
    /// DEGRADED so the caller knows the repository and preprint sweep
    /// Unpaywall adds did not run. Still includes the source line, so a
    /// reader knows nothing was downloaded.
    async fn degraded_open_access(&self, doi: &str) -> String {
        let fallback = || super::contact_email_missing_with_hint("Unpaywall", doi);
        if self.providers_available() {
            match self.license_record_for_doi(doi).await {
                Ok(Some(record)) => render_license_only(doi, &record),
                Ok(None) => fallback(),
                Err(message) => message,
            }
        } else {
            fallback()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::EnvGuard;
    use rmcp::model::{CallToolResult, ContentBlock};
    use rmcp::ServerHandler;
    use serde_json::json;
    use wiremock::matchers::{header, header_exists, method, path};
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
    async fn without_an_email_the_answer_degrades_to_crossref_license_and_url() {
        let _email = EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL");
        let doi = "10.5555/degraded.oa.1";
        let mock = MockServer::start().await;
        // Unpaywall must not be called when no email is set.
        Mock::given(method("GET"))
            .and(path(format!("/v2/{doi}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(unpaywall_gold()))
            .expect(0)
            .mount(&mock)
            .await;
        // Crossref holds a cc-by licence record for the work.
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", crate::server::encode_path_segment(doi))))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {
                    "type": "journal-article",
                    "DOI": doi,
                    "title": ["A Degraded-Path Open-Access Example"],
                    "container-title": ["Journal of Synthetic Fixtures"],
                    "URL": "https://journals.example.org/article/degraded.oa.1",
                    "resource": {"primary": {"URL": "https://journals.example.org/article/degraded.oa.1"}},
                    "license": [{"URL": "http://creativecommons.org/licenses/by/4.0/"}],
                    "issued": {"date-parts": [[2026]]}
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = Server::new_with_base(mock.uri())
            .with_discovery(mock.uri())
            .with_providers(mock.uri());
        let text = server.find_open_access_pdf(args(doi)).await;

        assert!(
            text.contains("[DEGRADED]"),
            "fallback must mark the answer as degraded: {text}"
        );
        assert!(
            text.starts_with(&format!("OPEN ACCESS doi:{doi}")),
            "cc-by licence plus landing URL is open access: {text}"
        );
        assert!(
            text.contains("A Degraded-Path Open-Access Example"),
            "title must come from the publisher record: {text}"
        );
        assert!(
            text.contains("creativecommons.org/licenses/by/4.0"),
            "licence must be shown: {text}"
        );
        assert!(
            text.contains("journals.example.org/article/degraded.oa.1"),
            "landing URL must be shown: {text}"
        );
        assert!(
            text.contains("set OOKCITE_MCP_CONTACT_EMAIL"),
            "hint for a fuller sweep must be present: {text}"
        );
        assert!(
            !text.contains("No request was made."),
            "a request was made (to Crossref), so the old sentence is gone: {text}"
        );
    }

    #[tokio::test]
    async fn without_an_email_a_closed_paper_still_names_the_publisher_landing_page() {
        let _email = EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL");
        let doi = "10.5555/degraded.closed.1";
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/works/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {
                    "type": "journal-article",
                    "DOI": doi,
                    "title": ["A paywalled work"],
                    "URL": "https://journals.example.org/paywalled/1",
                    "resource": {"primary": {"URL": "https://journals.example.org/paywalled/1"}},
                    "issued": {"date-parts": [[2026]]}
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = Server::new_with_base(mock.uri())
            .with_discovery(mock.uri())
            .with_providers(mock.uri());
        let text = server.find_open_access_pdf(args(doi)).await;

        assert!(
            text.starts_with(&format!("NO LEGAL COPY RECORDED doi:{doi}")),
            "no cc licence is not an open-access verdict: {text}"
        );
        assert!(text.contains("[DEGRADED]"), "{text}");
        assert!(
            text.contains("Licence: not stated in the publisher record."),
            "{text}"
        );
        assert!(
            text.contains("https://journals.example.org/paywalled/1"),
            "landing URL must still be shown so the caller has somewhere to go: {text}"
        );
    }

    #[tokio::test]
    async fn without_an_email_a_datacite_only_cc_licence_is_open_access_and_not_blank() {
        let _email = EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL");
        let doi = "10.5555/degraded.datacite.cc.1";
        let mock = MockServer::start().await;
        // Crossref has no record of this DOI (a dataset or a Zenodo
        // upload), so the fall-through reaches DataCite.
        Mock::given(method("GET"))
            .and(path(format!(
                "/works/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        // DataCite holds the work with a CC-BY-4.0 rights URL and a
        // landing URL under `data.attributes.url`.
        Mock::given(method("GET"))
            .and(path(format!(
                "/dois/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "attributes": {
                        "titles": [{"title": "A DataCite-only dataset"}],
                        "publicationYear": 2026,
                        "publisher": "Example Archive",
                        "url": "https://data.example.org/dataset/42",
                        "rightsList": [
                            {
                                "rights": "Creative Commons Attribution 4.0 International",
                                "rightsUri": "https://creativecommons.org/licenses/by/4.0/",
                                "rightsIdentifier": "CC-BY-4.0"
                            }
                        ]
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = Server::new_with_base(mock.uri())
            .with_discovery(mock.uri())
            .with_providers(mock.uri());
        let text = server.find_open_access_pdf(args(doi)).await;

        assert!(
            text.starts_with(&format!("OPEN ACCESS doi:{doi}")),
            "DataCite-only CC licence must count as open access, not NO LEGAL COPY RECORDED: {text}"
        );
        assert!(text.contains("[DEGRADED]"), "{text}");
        assert!(
            text.contains("https://creativecommons.org/licenses/by/4.0/"),
            "rightsUri must be printed: {text}"
        );
        assert!(
            text.contains("https://data.example.org/dataset/42"),
            "DataCite landing URL must be printed when Crossref has no record: {text}"
        );
        assert!(
            !text.contains("Licence: not stated in the publisher record."),
            "DataCite rights must not be reported as blank: {text}"
        );
    }

    #[tokio::test]
    async fn without_an_email_a_datacite_rights_identifier_is_canonicalised() {
        let _email = EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL");
        let doi = "10.5555/degraded.datacite.spdx.1";
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/works/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/dois/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "attributes": {
                        "titles": [{"title": "A dataset with only an SPDX identifier"}],
                        "publicationYear": 2026,
                        "url": "https://data.example.org/dataset/99",
                        "rightsList": [
                            {"rightsIdentifier": "CC-BY-SA-4.0"}
                        ]
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = Server::new_with_base(mock.uri())
            .with_discovery(mock.uri())
            .with_providers(mock.uri());
        let text = server.find_open_access_pdf(args(doi)).await;

        assert!(
            text.starts_with(&format!("OPEN ACCESS doi:{doi}")),
            "{text}"
        );
        assert!(
            text.contains("https://creativecommons.org/licenses/by-sa/4.0/"),
            "bare SPDX id must be canonicalised to the CC URL: {text}"
        );
    }

    #[tokio::test]
    async fn without_a_contact_email_it_says_so_and_makes_no_request() {
        for value in [
            None,
            Some("not-an-email"),
            Some("a b@example.org"),
            Some("ops@localhost"),
        ] {
            let _email = match value {
                Some(value) => EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", value),
                None => EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL"),
            };
            let mock = MockServer::start().await;
            Mock::given(wiremock::matchers::any())
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&mock)
                .await;

            let text = server(&mock).find_open_access_pdf(args(DOI)).await;

            assert!(
                text.starts_with(
                    "ERROR Unpaywall : OOKCITE_MCP_CONTACT_EMAIL is not set to an email address."
                ),
                "{value:?}: {text}"
            );
            assert!(text.contains("No request was made."), "{text}");
            assert!(!text.contains("rgoswami"), "{text}");
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            crate::server::promote_upstream_failure(&mut result);
            assert_eq!(result.is_error, Some(true));
        }
    }

    #[tokio::test]
    async fn the_operators_address_is_sent_as_email_and_in_the_user_agent() {
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", " ops@example.org ");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/{DOI}")))
            .and(|request: &Request| {
                request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "email" && value == "ops@example.org")
            })
            .and(header("user-agent", "ookcite-mcp (mailto:ops@example.org)"))
            .respond_with(ResponseTemplate::new(200).set_body_json(unpaywall_gold()))
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock).find_open_access_pdf(args(DOI)).await;

        assert!(text.starts_with("OPEN ACCESS doi:"), "{text}");
    }

    #[tokio::test]
    async fn a_transport_failure_carries_its_cause() {
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
        let server = Server::new_with_base("http://127.0.0.1:9".into())
            .with_discovery(crate::server::discovery::test_support::closed_base());

        let text = server.find_open_access_pdf(args(DOI)).await;

        assert!(
            text.starts_with("UNREACHABLE Unpaywall : could not connect to 127.0.0.1"),
            "{text}"
        );
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        crate::server::promote_upstream_failure(&mut result);
        assert_eq!(result.structured_content.unwrap()["kind"], "unreachable");
    }

    #[tokio::test]
    async fn a_402_keeps_its_retry_after_and_is_not_a_miss() {
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
        let _email = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
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
