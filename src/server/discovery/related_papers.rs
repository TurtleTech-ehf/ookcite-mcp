//! `related_papers`: neighbours of a paper in the citation graph, by DOI.
//!
//! OpenAlex supplies three views of one work: the works it lists as
//! related, the works it references, and the works that cite it. The tool
//! returns metadata and DOIs so the caller can verify and format each one.
//! A "related" list is OpenAlex's own similarity ranking. It is a lead for
//! reading, not evidence that two papers agree.

use std::collections::HashMap;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use super::{contact_email, doi_path, parse_doi_arg};
use crate::server::Server;

const SOURCE: &str = "OpenAlex";
const WORK_FIELDS: &str =
    "id,doi,display_name,publication_year,authorships,primary_location,cited_by_count";
const SEED_FIELDS: &str =
    "id,doi,display_name,publication_year,cited_by_count,related_works,referenced_works";
const MAX_LIMIT: u32 = 25;
const DEFAULT_LIMIT: u32 = 10;
const AUTHORS_SHOWN: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Works OpenAlex ranks as similar, from shared concepts and citations.
    #[default]
    Related,
    /// Works this paper cites, in the order OpenAlex lists them.
    References,
    /// Works that cite this paper, most cited first.
    CitedBy,
}

impl Relation {
    fn label(self) -> &'static str {
        match self {
            Self::Related => "related works",
            Self::References => "references",
            Self::CitedBy => "cited by",
        }
    }
}

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelatedPapersArgs {
    /// DOI of the seed paper: bare ("10.1038/187493a0"), "doi:" prefixed, or a doi.org URL.
    #[schemars(
        length(min = 7, max = 512),
        regex(
            pattern = r"^\s*((https?://(dx\.)?doi\.org/|[dD][oO][iI]:\s*))?10\.[0-9]{4,9}/\S+\s*$"
        )
    )]
    pub doi: String,
    /// Which neighbours to list. `related` (default) is OpenAlex's similarity list, `references` is what the paper cites, `cited_by` is what cites it.
    #[serde(default)]
    pub relation: Relation,
    /// How many papers to return, 1 to 25 (default 10).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 25))]
    pub limit: u32,
}

/// `W` followed by digits, from a bare id or an openalex.org URL. Anything
/// else is not put into a filter expression.
fn work_id(raw: &str) -> Option<String> {
    let tail = raw.trim().rsplit('/').next()?;
    let digits = tail.strip_prefix('W')?;
    (!digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())).then(|| tail.to_string())
}

fn bare_doi(raw: &str) -> Option<String> {
    crate::bib_audit::clean_doi(raw)
}

fn authors_line(work: &Value) -> String {
    let names: Vec<&str> = work["authorships"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry["author"]["display_name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return "authors not listed".into();
    }
    let shown = names
        .iter()
        .take(AUTHORS_SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > AUTHORS_SHOWN {
        format!("{shown} et al.")
    } else {
        shown
    }
}

fn work_line(index: usize, work: &Value) -> String {
    let title = work["display_name"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("(untitled)");
    let year = work["publication_year"]
        .as_i64()
        .map(|year| format!(" ({year})"))
        .unwrap_or_default();
    let venue = work["primary_location"]["source"]["display_name"]
        .as_str()
        .map(str::trim)
        .filter(|venue| !venue.is_empty())
        .unwrap_or("venue not listed");
    let doi = work["doi"]
        .as_str()
        .and_then(bare_doi)
        .map(|doi| format!("doi:{doi}"))
        .unwrap_or_else(|| {
            format!(
                "no DOI ({})",
                work["id"].as_str().unwrap_or("OpenAlex id missing")
            )
        });
    let cited = work["cited_by_count"]
        .as_u64()
        .map(|count| format!(" | cited by {count}"))
        .unwrap_or_default();
    format!(
        "{index}. {title}{year} -- {} | {venue} | {doi}{cited}",
        authors_line(work)
    )
}

/// The report for one seed and one relation. `works` is in display order;
/// `total` is how many OpenAlex lists for that relation; `unread` counts
/// listed works the second request did not return.
pub(super) fn render_related(
    doi: &str,
    relation: Relation,
    seed: &Value,
    works: &[Value],
    total: u64,
    unread: usize,
) -> String {
    let mut out = vec![format!(
        "{} for doi:{doi} (source: {SOURCE})",
        relation.label().to_uppercase()
    )];
    let seed_title = seed["display_name"].as_str().unwrap_or("(untitled)");
    let seed_year = seed["publication_year"]
        .as_i64()
        .map(|year| format!(" ({year})"))
        .unwrap_or_default();
    let seed_cited = seed["cited_by_count"]
        .as_u64()
        .map(|count| format!(" | cited by {count}"))
        .unwrap_or_default();
    out.push(format!("Seed: {seed_title}{seed_year}{seed_cited}"));
    if works.is_empty() {
        out.push(format!(
            "OpenAlex lists no {} for this work.",
            relation.label()
        ));
        return out.join("\n");
    }
    for (offset, work) in works.iter().enumerate() {
        out.push(work_line(offset + 1, work));
    }
    out.push(format!(
        "Showing {} of {total} {}.",
        works.len(),
        relation.label()
    ));
    if unread > 0 {
        out.push(format!(
            "{unread} listed work(s) could not be read from OpenAlex and are left out."
        ));
    }
    if relation == Relation::Related {
        out.push(
            "OpenAlex ranks related works by shared concepts and citations. Treat them as leads to read, \
             not as papers that agree with the seed."
                .into(),
        );
    }
    out.push(
        "Metadata only. Check a DOI with validate_doi before citing it, and format it with format_citation."
            .into(),
    );
    out.join("\n")
}

/// The request parameters every OpenAlex call carries, after `extra`.
fn openalex_params(
    extra: &[(&'static str, String)],
    email: &str,
    api_key: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut query = extra.to_vec();
    query.push(("mailto", email.to_string()));
    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        query.push(("api_key", key.to_string()));
    }
    query
}

impl Server {
    async fn openalex_get(
        &self,
        url: &str,
        extra: &[(&'static str, String)],
    ) -> Result<Option<Value>, String> {
        let api_key = std::env::var("OPENALEX_API_KEY").ok();
        let query = openalex_params(extra, &contact_email(), api_key.as_deref());
        self.discovery_json(SOURCE, url, &query).await
    }
}

#[tool_router(router = related_papers_tool_router, vis = "pub(super)")]
impl Server {
    #[tool(
        name = "related_papers",
        description = "List papers connected to a paper in the citation graph, by DOI. relation=related (default) returns works OpenAlex ranks as similar, relation=references returns what the paper cites, relation=cited_by returns what cites it, most cited first. Each result has title, year, first authors, venue, DOI, and citation count. Call this to widen a literature search from one known paper, or to see who built on it. Results are metadata only and OpenAlex's own ranking: validate_doi a DOI before citing it. NOT FOUND means OpenAlex has no record of the seed. A 429 or 402 from OpenAlex is reported with its Retry-After and is not an empty list. An OPENALEX_API_KEY in the environment is sent when set.",
        annotations(
            title = "Related papers",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn related_papers(&self, Parameters(args): Parameters<RelatedPapersArgs>) -> String {
        let doi = match parse_doi_arg(&args.doi) {
            Ok(doi) => doi,
            Err(message) => return message,
        };
        if !(1..=MAX_LIMIT).contains(&args.limit) {
            return format!("CLIENT ERROR: limit must be between 1 and {MAX_LIMIT}.");
        }
        let Some(bases) = self.discovery_bases() else {
            return format!("ERROR {SOURCE} : provider access is disabled");
        };
        let base = bases.openalex.clone();
        let seed_url = format!(
            "{}/works/doi:{}",
            base.trim_end_matches('/'),
            doi_path(&doi)
        );
        let seed = match self
            .openalex_get(&seed_url, &[("select", SEED_FIELDS.to_string())])
            .await
        {
            Ok(Some(seed)) => seed,
            Ok(None) => {
                return format!(
                    "NOT FOUND doi:{doi}\nOpenAlex has no record of this DOI, so there is no citation graph to read."
                );
            }
            Err(message) => return message,
        };
        let seed_id = work_id(seed["id"].as_str().unwrap_or(""));
        let limit = args.limit as usize;
        let works_url = format!("{}/works", base.trim_end_matches('/'));

        match args.relation {
            Relation::CitedBy => {
                let Some(seed_id) = seed_id else {
                    return format!("ERROR {SOURCE} : the seed record has no OpenAlex id");
                };
                let listed = match self
                    .openalex_get(
                        &works_url,
                        &[
                            ("filter", format!("cites:{seed_id}")),
                            ("sort", "cited_by_count:desc".to_string()),
                            ("per-page", args.limit.to_string()),
                            ("select", WORK_FIELDS.to_string()),
                        ],
                    )
                    .await
                {
                    Ok(Some(body)) => body,
                    Ok(None) => Value::Null,
                    Err(message) => return message,
                };
                let works: Vec<Value> = listed["results"].as_array().cloned().unwrap_or_default();
                let total = listed["meta"]["count"]
                    .as_u64()
                    .unwrap_or(works.len() as u64);
                render_related(&doi, args.relation, &seed, &works, total, 0)
            }
            Relation::Related | Relation::References => {
                let field = if args.relation == Relation::Related {
                    "related_works"
                } else {
                    "referenced_works"
                };
                let all: Vec<String> = seed[field]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(|id| id.as_str().and_then(work_id))
                            .collect()
                    })
                    .unwrap_or_default();
                let total = all.len() as u64;
                let wanted: Vec<String> = all.into_iter().take(limit).collect();
                if wanted.is_empty() {
                    return render_related(&doi, args.relation, &seed, &[], 0, 0);
                }
                let listed = match self
                    .openalex_get(
                        &works_url,
                        &[
                            ("filter", format!("openalex:{}", wanted.join("|"))),
                            ("per-page", wanted.len().to_string()),
                            ("select", WORK_FIELDS.to_string()),
                        ],
                    )
                    .await
                {
                    Ok(Some(body)) => body,
                    Ok(None) => Value::Null,
                    Err(message) => return message,
                };
                let mut by_id: HashMap<String, Value> = listed["results"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|work| {
                        work_id(work["id"].as_str().unwrap_or("")).map(|id| (id, work))
                    })
                    .collect();
                let works: Vec<Value> = wanted.iter().filter_map(|id| by_id.remove(id)).collect();
                let unread = wanted.len() - works.len();
                render_related(&doi, args.relation, &seed, &works, total, unread)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, ContentBlock};
    use rmcp::ServerHandler;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SEED_DOI: &str = "10.5555/demo.seed";

    fn server(mock: &MockServer) -> Server {
        Server::new_with_base("http://127.0.0.1:9".into()).with_discovery(mock.uri())
    }

    fn args(relation: Relation, limit: u32) -> Parameters<RelatedPapersArgs> {
        Parameters(RelatedPapersArgs {
            doi: SEED_DOI.into(),
            relation,
            limit,
        })
    }

    fn seed() -> Value {
        json!({
            "id": "https://openalex.org/W100",
            "doi": "https://doi.org/10.5555/demo.seed",
            "display_name": "A Mock Seed Paper",
            "publication_year": 2021,
            "cited_by_count": 120,
            "related_works": [
                "https://openalex.org/W201",
                "https://openalex.org/W202",
                "https://openalex.org/W203"
            ],
            "referenced_works": [
                "https://openalex.org/W301",
                "https://openalex.org/W302"
            ]
        })
    }

    fn work(id: &str, title: &str, year: i64, doi: Option<&str>, cited: u64) -> Value {
        json!({
            "id": format!("https://openalex.org/{id}"),
            "doi": doi.map(|doi| format!("https://doi.org/{doi}")),
            "display_name": title,
            "publication_year": year,
            "cited_by_count": cited,
            "authorships": [
                {"author": {"display_name": "Ada Example"}},
                {"author": {"display_name": "Ben Sample"}},
                {"author": {"display_name": "Chi Mock"}},
                {"author": {"display_name": "Dana Demo"}}
            ],
            "primary_location": {"source": {"display_name": "Journal of Mock Results"}}
        })
    }

    async fn mount_seed(mock: &MockServer) {
        Mock::given(method("GET"))
            .and(path(format!("/works/doi:{SEED_DOI}")))
            .and(query_param("select", SEED_FIELDS))
            .and(|request: &wiremock::Request| {
                request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "mailto" && value.contains('@'))
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(seed()))
            .expect(1)
            .mount(mock)
            .await;
    }

    fn assert_not_an_error(text: &str) {
        assert!(
            crate::http_error::upstream_failure_report(text).is_none(),
            "{text}"
        );
    }

    #[tokio::test]
    async fn related_works_come_back_in_openalex_rank_order() {
        let mock = MockServer::start().await;
        mount_seed(&mock).await;
        Mock::given(method("GET"))
            .and(path("/works"))
            .and(query_param("filter", "openalex:W201|W202"))
            .and(query_param("per-page", "2"))
            .and(query_param("select", WORK_FIELDS))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"count": 2},
                "results": [
                    work("W202", "Second Related Study", 2019, Some("10.5555/demo.rel2"), 8),
                    work("W201", "First Related Study", 2020, Some("10.5555/demo.rel1"), 41)
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::Related, 2))
            .await;

        let first = text.find("1. First Related Study (2020)").expect(&text);
        let second = text.find("2. Second Related Study (2019)").expect(&text);
        assert!(first < second, "{text}");
        assert!(
            text.starts_with("RELATED WORKS for doi:10.5555/demo.seed"),
            "{text}"
        );
        assert!(
            text.contains("Seed: A Mock Seed Paper (2021) | cited by 120"),
            "{text}"
        );
        assert!(
            text.contains("Ada Example, Ben Sample, Chi Mock et al."),
            "{text}"
        );
        assert!(
            text.contains("| Journal of Mock Results | doi:10.5555/demo.rel1 | cited by 41"),
            "{text}"
        );
        assert!(text.contains("Showing 2 of 3 related works."), "{text}");
        assert!(text.contains("Treat them as leads to read"), "{text}");
        assert_not_an_error(&text);
    }

    #[tokio::test]
    async fn references_use_the_referenced_list_and_flag_a_work_without_a_doi() {
        let mock = MockServer::start().await;
        mount_seed(&mock).await;
        Mock::given(method("GET"))
            .and(path("/works"))
            .and(query_param("filter", "openalex:W301|W302"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    work("W301", "A Cited Classic", 1998, Some("10.5555/demo.old"), 900),
                    work("W302", "A Cited Thesis", 2001, None, 3)
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::References, 10))
            .await;

        assert!(
            text.starts_with("REFERENCES for doi:10.5555/demo.seed"),
            "{text}"
        );
        assert!(text.contains("1. A Cited Classic (1998)"), "{text}");
        assert!(
            text.contains("no DOI (https://openalex.org/W302)"),
            "{text}"
        );
        assert!(text.contains("Showing 2 of 2 references."), "{text}");
        assert!(!text.contains("leads to read"), "{text}");
    }

    #[tokio::test]
    async fn cited_by_asks_for_the_most_cited_citers_and_reports_the_total() {
        let mock = MockServer::start().await;
        mount_seed(&mock).await;
        Mock::given(method("GET"))
            .and(path("/works"))
            .and(query_param("filter", "cites:W100"))
            .and(query_param("sort", "cited_by_count:desc"))
            .and(query_param("per-page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"count": 57},
                "results": [
                    work("W401", "A Heavily Cited Follow-up", 2023, Some("10.5555/demo.cite1"), 310),
                    work("W402", "A Later Follow-up", 2024, Some("10.5555/demo.cite2"), 12)
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::CitedBy, 2))
            .await;

        assert!(
            text.starts_with("CITED BY for doi:10.5555/demo.seed"),
            "{text}"
        );
        assert!(
            text.contains("1. A Heavily Cited Follow-up (2023)"),
            "{text}"
        );
        assert!(text.contains("Showing 2 of 57 cited by."), "{text}");
    }

    #[tokio::test]
    async fn a_listed_work_that_openalex_does_not_return_is_counted_not_hidden() {
        let mock = MockServer::start().await;
        mount_seed(&mock).await;
        Mock::given(method("GET"))
            .and(path("/works"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [work("W301", "A Cited Classic", 1998, Some("10.5555/demo.old"), 900)]
            })))
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::References, 10))
            .await;

        assert!(text.contains("1. A Cited Classic"), "{text}");
        assert!(
            text.contains("1 listed work(s) could not be read from OpenAlex"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_seed_with_no_related_works_makes_no_second_request() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/works/doi:{SEED_DOI}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "https://openalex.org/W100",
                "display_name": "A Mock Seed Paper",
                "related_works": []
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/works"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::Related, 10))
            .await;

        assert!(
            text.contains("OpenAlex lists no related works for this work."),
            "{text}"
        );
        assert_not_an_error(&text);
    }

    #[tokio::test]
    async fn an_unknown_seed_is_not_found_and_reads_nothing_else() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/works/doi:{SEED_DOI}")))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/works"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::Related, 10))
            .await;

        assert!(
            text.starts_with("NOT FOUND doi:10.5555/demo.seed"),
            "{text}"
        );
        assert_not_an_error(&text);
    }

    #[tokio::test]
    async fn a_429_on_the_seed_keeps_its_retry_after() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "45")
                    .set_body_json(json!({"message": "Rate limit exceeded"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::Related, 10))
            .await;

        assert!(
            text.starts_with("RATE LIMITED OpenAlex : 429 Too Many Requests"),
            "{text}"
        );
        let mut result = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        crate::server::promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["http_status"], 429);
        assert_eq!(structured["retry_after"], "45");
    }

    #[tokio::test]
    async fn a_402_on_the_second_request_is_reported_not_an_empty_list() {
        let mock = MockServer::start().await;
        mount_seed(&mock).await;
        Mock::given(method("GET"))
            .and(path("/works"))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("retry-after", "3600")
                    .set_body_json(json!({"message": "Budget exhausted"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let text = server(&mock)
            .related_papers(args(Relation::CitedBy, 5))
            .await;

        assert!(
            text.starts_with("PAYMENT REQUIRED OpenAlex : 402 Payment Required"),
            "{text}"
        );
        let report = crate::http_error::upstream_failure_report(&text).unwrap();
        assert_eq!(report.kind, "payment_required");
        assert_eq!(report.retry_after.as_deref(), Some("3600"));
        assert!(!text.contains("Showing"), "{text}");
    }

    #[tokio::test]
    async fn a_limit_outside_one_to_twenty_five_is_refused_before_any_request() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;
        let s = server(&mock);

        for limit in [0, 26] {
            let text = s.related_papers(args(Relation::Related, limit)).await;
            assert!(
                text.starts_with("CLIENT ERROR: limit must be between 1 and 25"),
                "{text}"
            );
        }
    }

    #[test]
    fn the_filter_only_ever_carries_openalex_work_ids() {
        assert_eq!(
            work_id("https://openalex.org/W2741809807").as_deref(),
            Some("W2741809807")
        );
        assert_eq!(work_id("W12").as_deref(), Some("W12"));
        assert!(work_id("W12|W13").is_none());
        assert!(work_id("https://openalex.org/A123").is_none());
        assert!(work_id("W").is_none());
        assert!(work_id("W1,publication_year:2020").is_none());
    }

    #[test]
    fn the_key_and_contact_address_are_added_only_when_present() {
        let extra = [("select", "id".to_string())];
        let keyless = openalex_params(&extra, "a@example.org", None);
        assert_eq!(
            keyless,
            vec![
                ("select", "id".to_string()),
                ("mailto", "a@example.org".to_string())
            ]
        );
        let blank = openalex_params(&extra, "a@example.org", Some("  "));
        assert_eq!(blank, keyless);
        let keyed = openalex_params(&extra, "a@example.org", Some(" k-123 "));
        assert_eq!(keyed.last(), Some(&("api_key", "k-123".to_string())));
    }

    #[test]
    fn the_defaults_are_related_and_ten() {
        let args: RelatedPapersArgs = serde_json::from_value(json!({"doi": SEED_DOI})).unwrap();
        assert_eq!(args.relation, Relation::Related);
        assert_eq!(args.limit, 10);
        let cited: RelatedPapersArgs =
            serde_json::from_value(json!({"doi": SEED_DOI, "relation": "cited_by"})).unwrap();
        assert_eq!(cited.relation, Relation::CitedBy);
        assert!(serde_json::from_value::<RelatedPapersArgs>(
            json!({"doi": SEED_DOI, "relation": "all"})
        )
        .is_err());
        assert!(
            serde_json::from_value::<RelatedPapersArgs>(json!({"doi": SEED_DOI, "extra": 1}))
                .is_err()
        );
    }

    #[test]
    fn the_input_schema_bounds_the_limit_and_names_every_relation() {
        let tool = Server::new_with_base("http://127.0.0.1:9".into())
            .get_tool("related_papers")
            .expect("tool is registered");
        let schema = Value::Object((*tool.input_schema).clone());
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["doi"]));
        let limit = &schema["properties"]["limit"];
        assert_eq!(limit["minimum"], 1);
        assert_eq!(limit["maximum"], 25);
        assert_eq!(limit["default"], 10);
        let text = schema.to_string();
        for relation in ["related", "references", "cited_by"] {
            assert!(text.contains(&format!("\"{relation}\"")), "{text}");
        }
        assert_eq!(schema["properties"]["doi"]["minLength"], 7);
    }

    #[test]
    fn the_readme_transcript_is_the_tool_output() {
        let readme = include_str!("../../../README.md");
        let works = [
            work(
                "W201",
                "First Related Study",
                2020,
                Some("10.5555/demo.rel1"),
                41,
            ),
            work(
                "W202",
                "Second Related Study",
                2019,
                Some("10.5555/demo.rel2"),
                8,
            ),
        ];
        let output = render_related(SEED_DOI, Relation::Related, &seed(), &works, 3, 0);
        assert!(
            readme.contains(&output),
            "README transcript for related_papers is stale; it should contain:\n{output}"
        );
    }
}
