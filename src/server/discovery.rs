//! Discovery tools: open-access copies, related work, claim-to-citation
//! search, and bibliography checks.
//!
//! These tools read two public indexes besides the OokCite API: Unpaywall
//! for open-access locations and OpenAlex for the citation graph. Both are
//! reached through a client that follows redirects only to its own hosts,
//! over https on port 443, and reads at most
//! [`crate::constants::PROVIDER_BODY_LIMIT`] bytes of a body. Every tool
//! returns links and metadata. None downloads a paper.

use serde_json::Value;

use super::{read_capped_body, ProviderRead, Server};
use crate::http_error::error_detail;

mod check_bibliography;
mod cite_while_writing;
mod open_access;
mod related_papers;

/// The operator's contact address from `OOKCITE_MCP_CONTACT_EMAIL`. Unpaywall
/// requires one and OpenAlex and Crossref ask for one, so they can write to an
/// operator whose traffic misbehaves. There is no default: the address must
/// be the operator's own. A value that is not an address counts as unset.
pub(super) fn contact_email() -> Option<String> {
    std::env::var("OOKCITE_MCP_CONTACT_EMAIL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| {
            let valid = value
                .split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
            valid && !value.contains(char::is_whitespace)
        })
}

/// User-Agent for every provider request. It names the operator's address when
/// one is set, and carries no address otherwise.
pub(super) fn provider_user_agent() -> String {
    match contact_email() {
        Some(email) => format!("ookcite-mcp (mailto:{email})"),
        None => "ookcite-mcp".to_string(),
    }
}

/// Tool text when a provider needs the operator's address and none is set.
pub(super) fn contact_email_missing(source: &str) -> String {
    format!(
        "ERROR {source} : OOKCITE_MCP_CONTACT_EMAIL is not set to an email address. {source} \
         requires a contact address with each request, and this server does not send one \
         unless you set it. Set OOKCITE_MCP_CONTACT_EMAIL to your own address in the server's \
         environment, then retry. No request was made."
    )
}

#[derive(Clone)]
pub(super) struct DiscoveryBases {
    pub(super) unpaywall: String,
    pub(super) openalex: String,
}

impl DiscoveryBases {
    pub(super) fn production() -> Self {
        Self {
            unpaywall: "https://api.unpaywall.org".into(),
            openalex: "https://api.openalex.org".into(),
        }
    }

    pub(super) fn hosts(&self) -> Vec<String> {
        [&self.unpaywall, &self.openalex]
            .into_iter()
            .filter_map(|base| reqwest::Url::parse(base).ok())
            .filter_map(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
            .collect()
    }

    #[cfg(any(test, feature = "demo-mock"))]
    pub(super) fn shared(base: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        Self {
            unpaywall: base.clone(),
            openalex: base,
        }
    }

    #[cfg(test)]
    fn test_origins(&self) -> Vec<crate::constants::TestProviderOrigin> {
        [&self.unpaywall, &self.openalex]
            .into_iter()
            .map(|base| crate::constants::TestProviderOrigin::parse(base))
            .collect()
    }
}

pub(super) fn discovery_client(
    bases: Option<&DiscoveryBases>,
    timeout_secs: u64,
) -> reqwest::Client {
    let hosts = bases.map(DiscoveryBases::hosts).unwrap_or_default();
    crate::constants::build_provider_client(timeout_secs, hosts)
}

/// A DOI as the tools accept it: bare, `doi:`-prefixed, or a doi.org URL.
/// Returns the lowercase bare DOI.
pub(super) fn parse_doi_arg(raw: &str) -> Result<String, String> {
    crate::bib_audit::clean_doi(raw).ok_or_else(|| {
        format!(
            "CLIENT ERROR: `{}` is not a DOI. Pass a bare DOI such as 10.1038/187493a0, a doi: \
             form, or a doi.org URL.",
            raw.trim().chars().take(80).collect::<String>()
        )
    })
}

/// Percent-encode a DOI for a provider path. The slash between the prefix
/// and the suffix stays, because both indexes route on it.
pub(super) fn doi_path(doi: &str) -> String {
    doi.split('/')
        .map(super::encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// A non-success provider answer: the status, and the body and `Retry-After`
/// as `error_detail` words them.
pub(super) struct ProviderStatus {
    host: String,
    status: u16,
    detail: String,
}

pub(super) async fn read_provider_status(resp: reqwest::Response) -> ProviderStatus {
    ProviderStatus {
        host: resp.url().host_str().unwrap_or("provider").to_string(),
        status: resp.status().as_u16(),
        detail: error_detail(resp).await,
    }
}

impl ProviderStatus {
    /// The host that answered, for a caller that adds the DOI it was reading.
    pub(super) fn host(&self) -> &str {
        &self.host
    }

    /// Tool text. The marker and the `Retry-After` value survive on the first
    /// line, so `call_tool` can mark the result as an error and report the
    /// status. `source` names who answered, and the DOI when there is one.
    pub(super) fn text(&self, source: &str) -> String {
        let detail = &self.detail;
        match self.status {
            402 => format!(
                "PAYMENT REQUIRED {source} : {detail}\n{source} answered 402, which its public \
                 API does not document. This server did not retry it. Wait for Retry-After if \
                 one was given; the OokCite quota is not involved."
            ),
            429 => format!(
                "RATE LIMITED {source} : {detail}\nThis limit belongs to {source}, not to \
                 OokCite. Wait for Retry-After before calling again."
            ),
            408 | 504 => format!("TIMEOUT {source} : {detail}"),
            401 | 403 => format!("ACCESS DENIED {source} : {detail}"),
            500..=599 => format!("TEMPORARY ERROR {source} : {detail}"),
            _ => format!("ERROR {source} : {detail}"),
        }
    }
}

pub(super) async fn provider_status_text(source: &str, resp: reqwest::Response) -> String {
    read_provider_status(resp).await.text(source)
}

impl Server {
    /// GET a provider URL and parse the JSON body.
    ///
    /// `Ok(None)` is a 404, which both indexes use for an unknown work. A
    /// throttle, a payment wall, or a restart is `Err` with its marker, so a
    /// caller cannot report it as a miss.
    pub(super) async fn discovery_json(
        &self,
        source: &str,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<Option<Value>, String> {
        if self.discovery.is_none() {
            return Err(format!("ERROR {source} : provider access is disabled"));
        }
        let sent = crate::resolve_helpers::send_idempotent_with_retry(|| {
            self.discovery_http
                .get(url)
                .query(query)
                .header(reqwest::header::USER_AGENT, provider_user_agent())
                .header(reqwest::header::ACCEPT, "application/json")
        })
        .await;
        let response = match sent {
            Ok(response) => response,
            Err(error)
                if error.is_redirect()
                    || error.to_string().contains("provider redirect refused") =>
            {
                return Err(format!("ERROR {source} : provider redirect refused"));
            }
            Err(error) => {
                return Err(format!(
                    "UNREACHABLE {source} : {}",
                    super::transport_cause(&error)
                ));
            }
        };
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(provider_status_text(source, response).await);
        }
        let bytes = match read_capped_body(response).await {
            Ok(bytes) => bytes,
            Err(ProviderRead::TooLarge) => {
                return Err(format!(
                    "ERROR {source} : response exceeded {} bytes",
                    crate::constants::PROVIDER_BODY_LIMIT
                ));
            }
            Err(ProviderRead::Redirect) => {
                return Err(format!("ERROR {source} : provider redirect refused"));
            }
            Err(ProviderRead::Failed(cause)) => {
                return Err(format!("UNREACHABLE {source} : {cause}"));
            }
            Err(ProviderRead::Status(status)) => return Err(status.text(source)),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(body) => Ok(Some(body)),
            Err(_) => Err(format!("ERROR {source} : the response was not JSON")),
        }
    }

    pub(super) fn discovery_bases(&self) -> Option<&DiscoveryBases> {
        self.discovery.as_ref()
    }
}

#[cfg(test)]
impl Server {
    /// Point Unpaywall and OpenAlex at one mock. The client may start on the
    /// mock's http origin and follows a redirect only back to that origin.
    pub(super) fn with_discovery(mut self, base: String) -> Self {
        let bases = DiscoveryBases::shared(base);
        self.discovery_http = crate::constants::build_provider_client_for_tests(
            5,
            std::iter::empty::<String>(),
            bases.test_origins(),
        );
        self.discovery = Some(bases);
        self
    }
}

pub(super) fn discovery_routers() -> rmcp::handler::server::tool::ToolRouter<Server> {
    Server::open_access_tool_router()
        + Server::related_papers_tool_router()
        + Server::cite_while_writing_tool_router()
        + Server::check_bibliography_tool_router()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_discovery_redirect_allowlist_is_the_two_index_hosts() {
        let hosts: HashSet<_> = DiscoveryBases::production().hosts().into_iter().collect();
        assert_eq!(
            hosts,
            HashSet::from([
                "api.unpaywall.org".to_string(),
                "api.openalex.org".to_string()
            ])
        );
    }

    #[test]
    fn a_doi_path_keeps_the_prefix_slash_and_encodes_the_rest() {
        assert_eq!(
            doi_path("10.1002/(sici)1097-4571(199806)49:8<693::aid-asi4>3.0.co;2-o"),
            "10.1002/%28sici%291097-4571%28199806%2949%3A8%3C693%3A%3Aaid-asi4%3E3.0.co%3B2-o"
        );
    }

    #[test]
    fn a_doi_argument_is_normalised_or_refused() {
        assert_eq!(
            parse_doi_arg(" https://doi.org/10.1038/NPHYS1170 ").unwrap(),
            "10.1038/nphys1170"
        );
        assert_eq!(parse_doi_arg("doi:10.1/x").unwrap(), "10.1/x");
        assert!(parse_doi_arg("nature12373")
            .unwrap_err()
            .starts_with("CLIENT ERROR:"));
    }

    #[tokio::test]
    async fn a_server_without_providers_says_so_instead_of_calling_out() {
        let server = Server::new_with_base("http://127.0.0.1:9".into());
        let error = server
            .discovery_json("Unpaywall", "https://api.unpaywall.org/v2/10.1/x", &[])
            .await
            .unwrap_err();
        assert_eq!(error, "ERROR Unpaywall : provider access is disabled");
    }
}

#[cfg(test)]
mod provider_status_tests {
    use super::*;
    use crate::server::tests::EnvGuard;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn server(mock: &MockServer) -> Server {
        Server::new_with_base(mock.uri()).with_providers(mock.uri())
    }

    fn enc(doi: &str) -> String {
        crate::server::encode_path_segment(doi)
    }

    async fn registered(mock: &MockServer, doi: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/handles/{}", enc(doi))))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": 1})))
            .mount(mock)
            .await;
    }

    async fn answer(mock: &MockServer, route: String, status: u16, retry_after: &str) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", retry_after)
                    .set_body_json(json!({"message": "walled"})),
            )
            .mount(mock)
            .await;
    }

    fn report(text: &str) -> (&'static str, Option<u16>, Option<String>) {
        let report = crate::http_error::upstream_failure_report(text).expect(text);
        (report.kind, report.http_status, report.retry_after)
    }

    #[tokio::test]
    async fn a_crossref_429_and_402_name_the_doi_and_keep_retry_after() {
        for (status, kind, retry) in [
            (429u16, "rate_limited", "30"),
            (402, "payment_required", "600"),
        ] {
            let mock = MockServer::start().await;
            answer(
                &mock,
                format!("/works/{}", enc("10.5555/x.1")),
                status,
                retry,
            )
            .await;

            let text = server(&mock)
                .provider_record("10.5555/x.1")
                .await
                .unwrap_err();

            assert!(text.contains("for doi:10.5555/x.1"), "{text}");
            assert_eq!(
                report(&text),
                (kind, Some(status), Some(retry.to_string())),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn a_datacite_402_names_the_doi_and_keeps_retry_after() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc("10.5555/x.2"))))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        answer(&mock, format!("/dois/{}", enc("10.5555/x.2")), 402, "900").await;

        let text = server(&mock)
            .provider_record("10.5555/x.2")
            .await
            .unwrap_err();

        assert!(
            text.starts_with("PAYMENT REQUIRED 127.0.0.1 for doi:10.5555/x.2 : 402"),
            "{text}"
        );
        assert_eq!(
            report(&text),
            ("payment_required", Some(402), Some("900".into()))
        );
    }

    #[tokio::test]
    async fn a_zenodo_429_names_the_doi() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc("10.5281/zenodo.7"))))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"message": {"DOI": "10.5281/zenodo.7"}})),
            )
            .mount(&mock)
            .await;
        answer(&mock, "/records/7".into(), 429, "11").await;

        let text = server(&mock)
            .provider_record("10.5281/zenodo.7")
            .await
            .unwrap_err();

        assert!(text.contains("for doi:10.5281/zenodo.7"), "{text}");
        assert_eq!(
            report(&text),
            ("rate_limited", Some(429), Some("11".into()))
        );
    }

    #[tokio::test]
    async fn a_doi_org_throttle_or_restart_is_not_an_unregistered_doi() {
        for (status, kind, http) in [
            (429u16, "rate_limited", 429u16),
            (503, "temporary_error", 503),
        ] {
            let mock = MockServer::start().await;
            answer(
                &mock,
                format!("/handles/{}", enc("10.5555/x.3")),
                status,
                "7",
            )
            .await;

            let error = server(&mock)
                .registration_status("10.5555/x.3")
                .await
                .unwrap_err();

            assert!(error.contains("for doi:10.5555/x.3"), "{error}");
            assert!(!error.contains("UNREGISTERED"), "{error}");
            assert_eq!(report(&error).0, kind);
            assert_eq!(report(&error).1, Some(http));
            assert_eq!(report(&error).2.as_deref(), Some("7"));
        }
    }

    #[tokio::test]
    async fn a_doi_org_answer_that_is_not_found_is_still_unregistered() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/handles/{}", enc("10.5555/x.4"))))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"responseCode": 100})))
            .mount(&mock)
            .await;

        assert_eq!(
            server(&mock).registration_status("10.5555/x.4").await,
            Ok(false)
        );
    }

    #[tokio::test]
    async fn a_bib_export_names_the_doi_whose_provider_read_failed() {
        let mock = MockServer::start().await;
        registered(&mock, "10.5555/x.5").await;
        answer(&mock, format!("/works/{}", enc("10.5555/x.5")), 429, "30").await;
        let entries = vec![json!({"metadata": {
            "doi": "10.5555/x.5", "title": "A stored title", "entry_type": "article"
        }})];

        let text = server(&mock)
            .render_collection_bib(&entries, crate::bib_export::BibOptions::default())
            .await;

        assert!(
            text.starts_with("RATE LIMITED 127.0.0.1 for doi:10.5555/x.5 : 429"),
            "{text}"
        );
        assert_eq!(
            report(&text),
            ("rate_limited", Some(429), Some("30".into()))
        );
    }

    #[tokio::test]
    async fn a_single_lookup_still_uses_the_ookcite_record_when_the_publisher_read_is_throttled() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        registered(&mock, "10.5555/x.6").await;
        answer(&mock, format!("/works/{}", enc("10.5555/x.6")), 402, "60").await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "doi": "10.5555/x.6", "title": "From the OokCite record"
            })))
            .mount(&mock)
            .await;

        let lookup = server(&mock)
            .lookup_doi_with_freshness("10.5555/x.6")
            .await
            .unwrap();

        assert_eq!(lookup.metadata["title"], "From the OokCite record");
    }
}

#[cfg(test)]
mod contact_email_tests {
    use super::*;
    use crate::server::tests::EnvGuard;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn only_a_real_address_counts_and_there_is_no_default() {
        for (value, expected) in [
            (Some("ops@example.org"), Some("ops@example.org")),
            (Some("  ops@example.org\n"), Some("ops@example.org")),
            (Some(""), None),
            (Some("ops"), None),
            (Some("@example.org"), None),
            (Some("ops@localhost"), None),
            (Some("o ps@example.org"), None),
            (None, None),
        ] {
            let _guard = match value {
                Some(value) => EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", value),
                None => EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL"),
            };
            assert_eq!(contact_email().as_deref(), expected, "{value:?}");
        }
    }

    #[test]
    fn the_user_agent_carries_the_operators_address_or_none() {
        {
            let _guard = EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", "ops@example.org");
            assert_eq!(
                provider_user_agent(),
                "ookcite-mcp (mailto:ops@example.org)"
            );
        }
        let _guard = EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL");
        assert_eq!(provider_user_agent(), "ookcite-mcp");
    }

    #[tokio::test]
    async fn the_crossref_read_uses_the_same_user_agent() {
        for (value, agent) in [
            (
                Some("ops@example.org"),
                "ookcite-mcp (mailto:ops@example.org)",
            ),
            (None, "ookcite-mcp"),
        ] {
            let _guard = match value {
                Some(value) => EnvGuard::set("OOKCITE_MCP_CONTACT_EMAIL", value),
                None => EnvGuard::unset("OOKCITE_MCP_CONTACT_EMAIL"),
            };
            let mock = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/works/10.5555%2Fua.1"))
                .and(header("user-agent", agent))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"message": {"DOI": "10.5555/ua.1"}})),
                )
                .expect(1)
                .mount(&mock)
                .await;
            let server = Server::new_with_base(mock.uri()).with_providers(mock.uri());

            let record = server.provider_record("10.5555/ua.1").await.unwrap();

            assert!(record.is_some(), "{value:?}");
        }
    }
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    use crate::server::tests::EnvGuard;
    use crate::tool_args::AuditBibliographyArgs;
    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REFUSALS: [u16; 5] = [400, 401, 403, 409, 422];

    fn server(mock: &MockServer) -> Server {
        Server::new_with_base(mock.uri()).with_providers(mock.uri())
    }

    async fn handle_answers(mock: &MockServer, doi: &str, status: u16, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/handles/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(mock)
            .await;
    }

    #[tokio::test]
    async fn a_refusal_from_doi_org_is_not_an_unregistered_doi() {
        for status in REFUSALS {
            let mock = MockServer::start().await;
            handle_answers(&mock, "10.5555/r.1", status, json!({"message": "nope"})).await;

            let result = server(&mock).registration_status("10.5555/r.1").await;

            let error = result.expect_err(&format!("HTTP {status} must not decide registration"));
            assert!(error.contains(&status.to_string()), "{error}");
            assert!(error.contains("doi:10.5555/r.1"), "{error}");
            assert!(!error.contains("UNREGISTERED"), "{error}");
            let required = server(&mock).require_registered("10.5555/r.1").await;
            assert!(!required.unwrap_err().starts_with("UNREGISTERED"));
        }
    }

    #[tokio::test]
    async fn only_a_not_found_answer_is_an_unregistered_doi() {
        let mock = MockServer::start().await;
        handle_answers(&mock, "10.5555/r.2", 404, json!({"responseCode": 100})).await;
        assert_eq!(
            server(&mock).registration_status("10.5555/r.2").await,
            Ok(false)
        );
        let mock = MockServer::start().await;
        handle_answers(&mock, "10.5555/r.3", 200, json!({"responseCode": 1})).await;
        assert_eq!(
            server(&mock).registration_status("10.5555/r.3").await,
            Ok(true)
        );
    }

    #[tokio::test]
    async fn audit_bibliography_reports_a_refusal_as_an_error_with_its_status() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        for status in REFUSALS {
            let mock = MockServer::start().await;
            handle_answers(&mock, "10.5555/r.5", status, json!({"message": "nope"})).await;
            let args = AuditBibliographyArgs {
                content: "@article{k, title={A paper that exists}, doi={10.5555/r.5}}".into(),
            };

            let text = server(&mock).audit_bibliography(Parameters(args)).await;

            assert!(
                text.contains("k | UPSTREAM | doi:10.5555/r.5"),
                "{status}: {text}"
            );
            assert!(!text.contains("BROKEN"), "{status}: {text}");
            let report = crate::http_error::upstream_failure_report(&text)
                .unwrap_or_else(|| panic!("{status} is not an error result: {text}"));
            assert_eq!(report.http_status, Some(status), "{status}: {text}");
        }
    }

    #[tokio::test]
    async fn audit_bibliography_marks_a_malformed_doi_field_broken_without_a_request() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;
        let args = AuditBibliographyArgs {
            content: "@article{k, title={Not a real identifier}, doi={soon}}".into(),
        };

        let text = server(&mock).audit_bibliography(Parameters(args)).await;

        assert!(
            text.contains("k | BROKEN | doi field is not a DOI: soon"),
            "{text}"
        );
        assert!(!text.contains("UPSTREAM"), "{text}");
    }

    #[tokio::test]
    async fn a_handle_answer_that_is_not_a_registration_verdict_is_an_error() {
        let doi = "10.5555/q.1";
        let errors: [(u16, &str); 6] = [
            (200, "<html>maintenance</html>"),
            (200, "{}"),
            (200, r#"{"responseCode":"1"}"#),
            (200, r#"{"responseCode":2}"#),
            (200, r#"{"responseCode":null}"#),
            (404, "<html>Not Found</html>"),
        ];
        for (status, body) in errors {
            let mock = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!(
                    "/handles/{}",
                    crate::server::encode_path_segment(doi)
                )))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&mock)
                .await;

            let result = server(&mock).registration_status(doi).await;

            let error = result.expect_err(&format!("{status} {body} decided registration"));
            assert!(error.starts_with("ERROR "), "{error}");
            assert!(error.contains("doi:10.5555/q.1"), "{error}");
        }
        let verdicts: [(u16, &str, bool); 3] = [
            (200, r#"{"responseCode":1.0}"#, true),
            (200, r#"{"responseCode":100}"#, false),
            (404, r#"{"responseCode":100}"#, false),
        ];
        for (status, body, registered) in verdicts {
            let mock = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!(
                    "/handles/{}",
                    crate::server::encode_path_segment(doi)
                )))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&mock)
                .await;
            assert_eq!(
                server(&mock).registration_status(doi).await,
                Ok(registered),
                "{status} {body}"
            );
        }
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use crate::server::ProviderBases;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(in crate::server) fn closed_base() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    /// doi.org answers on `handles`; Crossref, DataCite and Zenodo are at
    /// `elsewhere`. The client gives up after one second.
    pub(in crate::server) fn split(handles: &MockServer, elsewhere: &str) -> Server {
        let mut server = Server::new_with_base(handles.uri());
        let bases = ProviderBases {
            crossref: elsewhere.to_string(),
            datacite: elsewhere.to_string(),
            zenodo: elsewhere.to_string(),
            handles: handles.uri(),
        };
        server.provider_http = crate::constants::build_provider_client_for_tests(
            1,
            std::iter::empty::<String>(),
            bases.test_origins(),
        );
        server.providers = Some(bases);
        server
    }

    pub(in crate::server) async fn handle_ok(mock: &MockServer, doi: &str) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/handles/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": 1})))
            .mount(mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"doi": doi})))
            .mount(mock)
            .await;
    }

    pub(in crate::server) async fn slow_everything() -> MockServer {
        let slow = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(3)))
            .mount(&slow)
            .await;
        slow
    }
}

#[cfg(test)]
mod transport_tests {
    use super::test_support::{closed_base, slow_everything, split};
    use wiremock::MockServer;

    fn kind(text: &str) -> &'static str {
        crate::http_error::upstream_failure_report(text)
            .expect(text)
            .kind
    }

    #[tokio::test]
    async fn a_refused_connection_to_crossref_names_the_doi_and_the_cause() {
        let mock = MockServer::start().await;
        let error = split(&mock, &closed_base())
            .provider_record("10.5555/t.1")
            .await
            .unwrap_err();

        assert!(
            error.starts_with("UNREACHABLE 10.5555/t.1 : could not connect to 127.0.0.1"),
            "{error}"
        );
        assert!(!error.contains("error sending request"), "{error}");
        assert_eq!(kind(&error), "unreachable");
    }

    #[tokio::test]
    async fn a_timeout_reading_crossref_says_it_timed_out() {
        let mock = MockServer::start().await;
        let slow = slow_everything().await;

        let error = split(&mock, &slow.uri())
            .provider_record("10.5555/t.2")
            .await
            .unwrap_err();

        assert!(
            error.starts_with("UNREACHABLE 10.5555/t.2 : timed out reading 127.0.0.1"),
            "{error}"
        );
        assert_eq!(kind(&error), "unreachable");
    }

    #[tokio::test]
    async fn a_timeout_or_refusal_at_doi_org_is_unreachable_not_unregistered() {
        let slow = slow_everything().await;
        let error = split(&slow, &closed_base())
            .registration_status("10.5555/t.3")
            .await
            .unwrap_err();
        assert!(
            error.starts_with("UNREACHABLE 10.5555/t.3 : timed out reading 127.0.0.1"),
            "{error}"
        );

        let mut refused = split(&slow, &closed_base());
        refused.providers.as_mut().unwrap().handles = closed_base();
        let error = refused
            .registration_status("10.5555/t.3")
            .await
            .unwrap_err();
        assert!(error.contains("could not connect to 127.0.0.1"), "{error}");
        assert_eq!(kind(&error), "unreachable");
    }
}
