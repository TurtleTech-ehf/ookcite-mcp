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

use super::{read_capped_body, ProviderRead, Server, PROVIDER_USER_AGENT};
use crate::http_error::error_detail;

mod open_access;

/// Contact address sent to Unpaywall and OpenAlex. Both ask for one so they
/// can write to an operator whose traffic misbehaves. `OOKCITE_MCP_CONTACT_EMAIL`
/// replaces it for a deployment that has its own.
const DEFAULT_CONTACT_EMAIL: &str = "rgoswami@ieee.org";

pub(super) fn contact_email() -> String {
    std::env::var("OOKCITE_MCP_CONTACT_EMAIL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| value.contains('@') && !value.contains(char::is_whitespace))
        .unwrap_or_else(|| DEFAULT_CONTACT_EMAIL.to_string())
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

    #[cfg(test)]
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

/// Tool text for a non-success provider response. The marker and the
/// `Retry-After` value survive, so `call_tool` can mark the result as an
/// error and report the status.
pub(super) async fn provider_status_text(source: &str, resp: reqwest::Response) -> String {
    let status = resp.status();
    let detail = error_detail(resp).await;
    match status.as_u16() {
        402 => format!(
            "PAYMENT REQUIRED {source} : {detail}\n{source} answered 402, which its public API \
             does not document. This server did not retry it. Wait for Retry-After if one was \
             given; the OokCite quota is not involved."
        ),
        429 => format!(
            "RATE LIMITED {source} : {detail}\nThis limit belongs to {source}, not to OokCite. \
             Wait for Retry-After before calling again."
        ),
        408 | 504 => format!("TIMEOUT {source} : {detail}"),
        401 | 403 => format!("ACCESS DENIED {source} : {detail}"),
        500..=599 => format!("TEMPORARY ERROR {source} : {detail}"),
        _ => format!("ERROR {source} : {detail}"),
    }
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
                .header(reqwest::header::USER_AGENT, PROVIDER_USER_AGENT)
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
            Err(_) => return Err(format!("ERROR {source} : error sending request")),
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
            Err(ProviderRead::Failed) => {
                return Err(format!("ERROR {source} : error reading response"));
            }
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
