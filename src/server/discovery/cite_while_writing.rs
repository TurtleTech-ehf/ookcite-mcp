//! `cite_while_writing`: candidate citations for one claim sentence.
//!
//! The claim is cut down to its content words and searched in the OokCite
//! corpus. Each hit is scored by how many claim words its title and venue
//! share, and every reason shown is a measurement the caller can read:
//! the shared words, the corpus score, the year. The tool does not judge
//! whether a paper supports the claim, and says so. A candidate whose
//! publisher record carries a retraction, withdrawal, or expression of
//! concern is held back and named, because the commonest way to cite a
//! bad paper is to find it by keyword.

use std::collections::HashSet;

use futures::{stream, StreamExt};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use crate::batch_limits::read_only_concurrency;
use crate::bib_audit::{notices_from_provider, PublisherNotice};
use crate::http_error::failure_text;
use crate::server::{provider_failure, read_capped_body, Server};
use ookcite_mcp::endpoints;

const DEFAULT_LIMIT: u32 = 5;
const MAX_LIMIT: u32 = 10;
const MAX_QUERY_TERMS: usize = 12;
const MIN_QUERY_TERMS: usize = 2;
const MIN_CLAIM_CHARS: usize = 12;
const MAX_CLAIM_CHARS: usize = 2000;
const CLAIM_ECHO_CHARS: usize = 140;
/// Corpus hits read per requested candidate. Hits that have no DOI, are
/// too old, or are held back for a notice are replaced from this reserve.
const HITS_PER_CANDIDATE: u32 = 4;
const MAX_HITS: u32 = 40;

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiteWhileWritingArgs {
    /// The sentence or claim that needs a citation, as written in the draft (12 to 2000 characters).
    #[schemars(length(min = 12, max = 2000))]
    pub claim: String,
    /// How many candidates to return, 1 to 10 (default 5).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 10))]
    pub limit: u32,
    /// Drop candidates published before this year.
    #[serde(default)]
    #[schemars(range(min = 1000, max = 2100))]
    pub year_from: Option<i32>,
    /// Read each candidate's Crossref or DataCite record and hold back a retraction, withdrawal, or expression of concern (default true).
    #[serde(default = "default_true")]
    pub check_retractions: bool,
}

const STOPWORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "all",
    "also",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "between",
    "both",
    "but",
    "by",
    "can",
    "could",
    "did",
    "do",
    "does",
    "doing",
    "done",
    "during",
    "each",
    "either",
    "even",
    "few",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "him",
    "his",
    "how",
    "however",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "many",
    "may",
    "me",
    "might",
    "more",
    "most",
    "much",
    "must",
    "my",
    "no",
    "nor",
    "not",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "out",
    "over",
    "own",
    "same",
    "several",
    "shall",
    "she",
    "should",
    "so",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "then",
    "there",
    "therefore",
    "these",
    "they",
    "this",
    "those",
    "through",
    "thus",
    "to",
    "too",
    "under",
    "until",
    "up",
    "upon",
    "us",
    "very",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "whether",
    "which",
    "while",
    "who",
    "whom",
    "whose",
    "why",
    "will",
    "with",
    "within",
    "without",
    "would",
    "yet",
    "you",
    "your",
    "paper",
    "papers",
    "study",
    "studies",
    "show",
    "shows",
    "showed",
    "shown",
    "using",
    "used",
    "use",
    "based",
    "recent",
    "recently",
    "known",
    "well",
    "via",
];

fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

/// Content words of a claim, in the order they first appear, lowercased.
/// A word of one or two letters is kept only when the author wrote it as an
/// acronym or with a digit ("AI", "CO2"); otherwise it is noise.
pub(super) fn claim_terms(claim: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for raw in claim.split(|ch: char| !ch.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let word = raw.to_lowercase();
        let acronym = raw.chars().count() >= 2
            && (raw
                .chars()
                .all(|ch| ch.is_uppercase() || ch.is_ascii_digit())
                || raw.chars().any(|ch| ch.is_ascii_digit()));
        let long_enough = word.chars().count() >= 3;
        if !(long_enough || acronym) || is_stopword(&word) {
            continue;
        }
        if raw.chars().all(|ch| ch.is_ascii_digit()) && word.len() != 4 {
            continue;
        }
        if seen.insert(word.clone()) {
            terms.push(word);
        }
        if terms.len() == MAX_QUERY_TERMS {
            break;
        }
    }
    terms
}

/// Two words match when they are equal, differ by a plural "s", or share a
/// six-letter stem ("nucleation" and "nucleating").
fn same_word(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    for suffix in ["s", "es"] {
        if left.strip_suffix(suffix) == Some(right) || right.strip_suffix(suffix) == Some(left) {
            return true;
        }
    }
    let stem = |word: &str| word.chars().take(6).collect::<String>();
    left.chars().count() >= 6 && right.chars().count() >= 6 && stem(left) == stem(right)
}

fn words_of(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Claim terms that appear, in the same or a stemmed form, in `text`.
pub(super) fn shared_terms<'a>(terms: &'a [String], text: &str) -> Vec<&'a str> {
    let words = words_of(text);
    terms
        .iter()
        .filter(|term| words.iter().any(|word| same_word(term, word)))
        .map(String::as_str)
        .collect()
}

#[derive(Debug, Clone)]
pub(super) struct Candidate {
    pub(super) title: String,
    pub(super) doi: String,
    pub(super) year: Option<i64>,
    pub(super) journal: Option<String>,
    pub(super) score: Option<f64>,
    pub(super) shared: Vec<String>,
    pub(super) term_count: usize,
}

impl Candidate {
    fn coverage(&self) -> f64 {
        if self.term_count == 0 {
            0.0
        } else {
            self.shared.len() as f64 / self.term_count as f64
        }
    }

    fn match_label(&self) -> &'static str {
        match self.coverage() {
            c if c >= 0.5 => "high",
            c if c >= 0.25 => "medium",
            _ => "low",
        }
    }
}

/// Candidates from a corpus response, scored against the claim terms and
/// ordered by shared words, then corpus score. Hits with no DOI cannot be
/// verified or formatted, so they are counted and left out.
pub(super) fn candidates_from(
    data: &Value,
    terms: &[String],
    year_from: Option<i32>,
) -> (Vec<Candidate>, usize, usize) {
    let mut kept = Vec::new();
    let mut no_doi = 0usize;
    let mut too_old = 0usize;
    let mut seen = HashSet::new();
    for hit in data["matches"].as_array().cloned().unwrap_or_default() {
        let meta = &hit["metadata"];
        let title = meta["title"].as_str().map(str::trim).unwrap_or("");
        let doi = meta["doi"]
            .as_str()
            .and_then(crate::bib_audit::clean_doi)
            .unwrap_or_default();
        if title.is_empty() || doi.is_empty() {
            no_doi += 1;
            continue;
        }
        if !seen.insert(doi.clone()) {
            continue;
        }
        let year = meta["date"]["year"].as_i64();
        if let (Some(from), Some(year)) = (year_from, year) {
            if year < i64::from(from) {
                too_old += 1;
                continue;
            }
        }
        let journal = meta["journal"]
            .as_str()
            .map(str::trim)
            .filter(|journal| !journal.is_empty() && *journal != "N/A")
            .map(str::to_string);
        let searchable = format!("{title} {}", journal.as_deref().unwrap_or(""));
        let shared = shared_terms(terms, &searchable)
            .into_iter()
            .map(str::to_string)
            .collect();
        kept.push(Candidate {
            title: title.to_string(),
            doi,
            year,
            journal,
            score: hit["score"].as_f64(),
            shared,
            term_count: terms.len(),
        });
    }
    kept.sort_by(|left, right| {
        right.shared.len().cmp(&left.shared.len()).then_with(|| {
            right
                .score
                .unwrap_or(0.0)
                .partial_cmp(&left.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    (kept, no_doi, too_old)
}

#[derive(Debug, Clone)]
pub(super) enum Screen {
    Clear,
    Held(Vec<String>),
    Unchecked(String),
    Skipped,
}

/// Read the corpus answer, refusing a body past [`PROVIDER_BODY_LIMIT`] while
/// it streams, as every provider read does, and refusing one that is not JSON
/// rather than reading it as an empty result.
async fn corpus_body(response: reqwest::Response) -> Result<Value, String> {
    let bytes = read_capped_body(response)
        .await
        .map_err(|error| provider_failure("corpus", error))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| "ERROR corpus : the response was not valid JSON".to_string())
}

pub(super) fn screen_from_provider(provider: Result<Option<Value>, String>) -> Screen {
    match provider {
        Err(error) => Screen::Unchecked(error),
        Ok(None) => Screen::Unchecked("no Crossref or DataCite record to read".into()),
        Ok(Some(record)) => {
            let notices: Vec<PublisherNotice> = notices_from_provider(Some(&record));
            let serious: Vec<String> = notices
                .iter()
                .filter(|notice| !matches!(notice.kind, crate::bib_audit::NoticeKind::Correction))
                .map(PublisherNotice::line)
                .collect();
            if serious.is_empty() {
                Screen::Clear
            } else {
                Screen::Held(serious)
            }
        }
    }
}

fn clip(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        let cut: String = flat.chars().take(limit).collect();
        format!("{}...", cut.trim_end())
    }
}

pub(super) fn render_candidates(
    claim: &str,
    terms: &[String],
    screened: &[(Candidate, Screen)],
    limit: usize,
    skipped: (usize, usize),
) -> String {
    let mut out = vec![
        format!(
            "CITATION CANDIDATES for: \"{}\"",
            clip(claim, CLAIM_ECHO_CHARS)
        ),
        format!("Searched the OokCite corpus for: {}", terms.join(" ")),
    ];
    let offered: Vec<&(Candidate, Screen)> = screened
        .iter()
        .filter(|(_, screen)| !matches!(screen, Screen::Held(_)))
        .take(limit)
        .collect();
    if offered.is_empty() {
        out.push("No usable candidates. Reword the claim with the field's own terms, or search by author with reverse_lookup.".into());
    }
    for (index, (candidate, screen)) in offered.iter().enumerate() {
        let year = candidate
            .year
            .map(|year| format!(" ({year})"))
            .unwrap_or_default();
        let venue = candidate
            .journal
            .as_deref()
            .map(|journal| format!(" | {journal}"))
            .unwrap_or_default();
        out.push(format!(
            "{}. {}{year}{venue} | doi:{}",
            index + 1,
            candidate.title,
            candidate.doi
        ));
        let mut why = if candidate.shared.is_empty() {
            "Why: the corpus index matched it, but its title shares no claim word; read the abstract before using it".to_string()
        } else {
            format!(
                "Why: title and venue share {} of {} claim words ({}), title match {}",
                candidate.shared.len(),
                candidate.term_count,
                candidate.shared.join(", "),
                candidate.match_label()
            )
        };
        if let Some(score) = candidate.score {
            why.push_str(&format!("; corpus score {score:.0}"));
        }
        out.push(format!("   {why}"));
        out.push(format!(
            "   Notices: {}",
            match screen {
                Screen::Clear =>
                    "no retraction, withdrawal, or concern in the publisher record".to_string(),
                Screen::Unchecked(reason) => format!(
                    "UNCHECKED ({})",
                    reason.lines().next().unwrap_or("no reason given")
                ),
                Screen::Skipped => "not checked (check_retractions was false)".to_string(),
                Screen::Held(_) => unreachable!("held candidates are not offered"),
            }
        ));
    }
    let held: Vec<&(Candidate, Screen)> = screened
        .iter()
        .filter(|(_, screen)| matches!(screen, Screen::Held(_)))
        .collect();
    for (candidate, screen) in held {
        if let Screen::Held(lines) = screen {
            out.push(format!(
                "HELD BACK doi:{} \"{}\": {}. Do not cite it as sound work.",
                candidate.doi,
                clip(&candidate.title, 80),
                lines.join("; ")
            ));
        }
    }
    let (no_doi, too_old) = skipped;
    if no_doi + too_old > 0 {
        out.push(format!(
            "Left out: {no_doi} hit(s) with no DOI, {too_old} older than year_from."
        ));
    }
    let unchecked = offered
        .iter()
        .filter(|(_, screen)| matches!(screen, Screen::Unchecked(_)))
        .count();
    if unchecked > 0 {
        out.push(format!(
            "{unchecked} candidate(s) above are UNCHECKED: their publisher record could not be read, so they are not called clean."
        ));
    }
    out.push(
        "These are keyword matches on titles and the corpus index. None is evidence that the paper \
         supports the claim: read it, then validate_doi and format_citation the one you use."
            .into(),
    );
    out.join("\n")
}

#[tool_router(router = cite_while_writing_tool_router, vis = "pub(super)")]
impl Server {
    #[tool(
        name = "cite_while_writing",
        description = "Suggest papers to cite for one claim sentence, with the reason for each. Call this while drafting, when a sentence needs a citation and the user has no reference in mind. The claim is cut to its content words and searched in the OokCite corpus; each candidate shows the claim words its title shares, its corpus score, year, venue, and DOI. Candidates whose Crossref or DataCite record carries a retraction, withdrawal, or expression of concern are held back and named. Matches are by keyword, not by meaning: none is evidence that the paper supports the claim, so read it before citing. Use year_from to drop old work. For a known paper use reverse_lookup, and format_citation to render the one chosen. A 429 or 402 is reported with its Retry-After and is not an empty result.",
        annotations(
            title = "Cite while writing",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn cite_while_writing(
        &self,
        Parameters(args): Parameters<CiteWhileWritingArgs>,
    ) -> String {
        let claim = args.claim.trim();
        let chars = claim.chars().count();
        if !(MIN_CLAIM_CHARS..=MAX_CLAIM_CHARS).contains(&chars) {
            return format!(
                "CLIENT ERROR: claim must be {MIN_CLAIM_CHARS} to {MAX_CLAIM_CHARS} characters; got {chars}."
            );
        }
        if !(1..=MAX_LIMIT).contains(&args.limit) {
            return format!("CLIENT ERROR: limit must be between 1 and {MAX_LIMIT}.");
        }
        let terms = claim_terms(claim);
        if terms.len() < MIN_QUERY_TERMS {
            return "CLIENT ERROR: the claim has too few content words to search. Pass the whole sentence, not a fragment."
                .into();
        }

        let hits = (args.limit * HITS_PER_CANDIDATE).min(MAX_HITS).to_string();
        let query = terms.join(" ");
        let response = self
            .send_read(|| {
                self.request(endpoints::SEARCH_ENHANCED, &[]).query(&[
                    ("q", query.as_str()),
                    ("limit", hits.as_str()),
                    ("offset", "0"),
                ])
            })
            .await;
        let data: Value = match response {
            Ok(response) if response.status().is_success() => match corpus_body(response).await {
                Ok(data) => data,
                Err(text) => return text,
            },
            Ok(response) => return failure_text(response, "Failed to search the corpus").await,
            Err(error) => return format!("Failed to search the corpus: {error}"),
        };

        let (ranked, no_doi, too_old) = candidates_from(&data, &terms, args.year_from);
        let limit = args.limit as usize;
        let screened: Vec<(Candidate, Screen)> = if !args.check_retractions {
            ranked
                .into_iter()
                .map(|candidate| (candidate, Screen::Skipped))
                .collect()
        } else if self.providers.is_none() {
            ranked
                .into_iter()
                .map(|candidate| {
                    (
                        candidate,
                        Screen::Unchecked("provider access is disabled".into()),
                    )
                })
                .collect()
        } else {
            // Twice the request: a held-back hit is replaced from the rest.
            let head: Vec<Candidate> = ranked.into_iter().take(limit * 2).collect();
            stream::iter(head.into_iter().map(|candidate| {
                let server = self.clone();
                async move {
                    let provider = server.provider_record(&candidate.doi).await;
                    (candidate, screen_from_provider(provider))
                }
            }))
            .buffered(read_only_concurrency())
            .collect()
            .await
        };
        render_candidates(claim, &terms, &screened, limit, (no_doi, too_old))
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

    const CLAIM: &str =
        "Sheared colloidal glasses nucleate crystals faster than quiescent suspensions do.";

    fn server(mock: &MockServer) -> Server {
        Server::new_with_base(mock.uri()).with_providers(mock.uri())
    }

    fn plain_server(mock: &MockServer) -> Server {
        Server::new_with_base(mock.uri())
    }

    fn args(claim: &str) -> CiteWhileWritingArgs {
        CiteWhileWritingArgs {
            claim: claim.into(),
            limit: 2,
            year_from: None,
            check_retractions: true,
        }
    }

    fn hit(title: &str, doi: Option<&str>, year: i64, journal: &str, score: f64) -> Value {
        json!({
            "score": score,
            "metadata": {
                "title": title,
                "doi": doi,
                "journal": journal,
                "date": {"year": year}
            }
        })
    }

    fn corpus_hits() -> Value {
        json!({
            "total": 5,
            "offset": 0,
            "matches": [
                hit("Quiescent suspensions of hard spheres", Some("10.5555/demo.b"), 2015, "Soft Matter Demo", 90.0),
                hit("Crystal nucleation in sheared colloidal glasses", Some("10.5555/demo.a"), 2021, "Journal of Mock Physics", 40.0),
                hit("A review without an identifier", None, 2020, "Review Demo", 99.0),
                hit("Early shear experiments on colloids", Some("10.5555/demo.old"), 1979, "Old Demo", 70.0)
            ]
        })
    }

    async fn mount_corpus(mock: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path("/api/v1/search/enhanced"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(mock)
            .await;
    }

    async fn mount_crossref(mock: &MockServer, doi: &str, message: Value) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/works/{}",
                crate::server::encode_path_segment(doi)
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"status": "ok", "message": message})),
            )
            .expect(1)
            .mount(mock)
            .await;
    }

    #[test]
    fn claim_terms_keep_content_words_acronyms_and_years() {
        assert_eq!(
            claim_terms("The CO2 uptake of MOF-5 in 2019 is not an AI problem, as we showed."),
            ["co2", "uptake", "mof", "2019", "ai", "problem"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert!(claim_terms("It is what it is.").is_empty());
        let long = (0..40)
            .map(|n| format!("term{n}x"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(claim_terms(&long).len(), MAX_QUERY_TERMS);
    }

    #[test]
    fn words_match_by_plural_and_a_six_letter_stem_only() {
        assert!(same_word("glasses", "glass"));
        assert!(same_word("suspension", "suspensions"));
        assert!(same_word("nucleate", "nucleation"));
        assert!(!same_word("graphene", "graphite"));
        assert!(!same_word("cat", "category"));
        assert!(!same_word("model", "modular"));
    }

    #[tokio::test]
    async fn candidates_are_ranked_by_shared_words_and_say_why() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search/enhanced"))
            .and(query_param("q", claim_terms(CLAIM).join(" ")))
            .and(query_param("limit", "8"))
            .respond_with(ResponseTemplate::new(200).set_body_json(corpus_hits()))
            .expect(1)
            .mount(&mock)
            .await;
        mount_crossref(&mock, "10.5555/demo.a", json!({"DOI": "10.5555/demo.a"})).await;
        mount_crossref(&mock, "10.5555/demo.b", json!({"DOI": "10.5555/demo.b"})).await;
        mount_crossref(
            &mock,
            "10.5555/demo.old",
            json!({"DOI": "10.5555/demo.old"}),
        )
        .await;

        let text = server(&mock)
            .cite_while_writing(Parameters(args(CLAIM)))
            .await;

        let a = text
            .find("1. Crystal nucleation in sheared colloidal glasses (2021)")
            .expect(&text);
        let b = text
            .find("2. Quiescent suspensions of hard spheres (2015)")
            .expect(&text);
        assert!(a < b, "{text}");
        assert!(
            text.contains("| Journal of Mock Physics | doi:10.5555/demo.a"),
            "{text}"
        );
        assert!(
            text.contains(
                "share 5 of 8 claim words (sheared, colloidal, glasses, nucleate, crystals)"
            ),
            "{text}"
        );
        assert!(text.contains("corpus score 40"), "{text}");
        assert!(
            text.contains("Notices: no retraction, withdrawal, or concern in the publisher record"),
            "{text}"
        );
        assert!(
            text.contains("Left out: 1 hit(s) with no DOI, 0 older than year_from."),
            "{text}"
        );
        assert!(
            text.contains("None is evidence that the paper supports the claim"),
            "{text}"
        );
        assert!(!text.contains("A review without an identifier"), "{text}");
        assert!(
            crate::http_error::upstream_failure_report(&text).is_none(),
            "{text}"
        );
    }

    #[tokio::test]
    async fn year_from_drops_older_hits_and_counts_them() {
        let mock = MockServer::start().await;
        mount_corpus(&mock, corpus_hits()).await;
        let mut request = args(CLAIM);
        request.year_from = Some(2000);
        request.check_retractions = false;

        let text = plain_server(&mock)
            .cite_while_writing(Parameters(request))
            .await;

        assert!(!text.contains("Early shear experiments"), "{text}");
        assert!(
            text.contains("1 hit(s) with no DOI, 1 older than year_from"),
            "{text}"
        );
        assert!(
            text.contains("Notices: not checked (check_retractions was false)"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_retracted_candidate_is_held_back_and_replaced_from_the_reserve() {
        let mock = MockServer::start().await;
        mount_corpus(&mock, corpus_hits()).await;
        mount_crossref(
            &mock,
            "10.5555/demo.a",
            json!({
                "DOI": "10.5555/demo.a",
                "update-to": [{"type": "retraction", "DOI": "10.5555/notice", "updated": {"date-parts": [[2022, 3]]}}]
            }),
        )
        .await;
        mount_crossref(&mock, "10.5555/demo.b", json!({"DOI": "10.5555/demo.b"})).await;
        mount_crossref(
            &mock,
            "10.5555/demo.old",
            json!({"DOI": "10.5555/demo.old"}),
        )
        .await;

        let text = server(&mock)
            .cite_while_writing(Parameters(args(CLAIM)))
            .await;

        assert!(
            text.contains("HELD BACK doi:10.5555/demo.a \"Crystal nucleation in sheared colloidal glasses\": retracted by doi:10.5555/notice on 2022-03"),
            "{text}"
        );
        assert!(
            text.contains("1. Quiescent suspensions of hard spheres (2015)"),
            "{text}"
        );
        assert!(!text.contains("1. Crystal nucleation"), "{text}");
    }

    #[tokio::test]
    async fn a_throttled_publisher_read_leaves_the_candidate_unchecked_with_its_retry_after() {
        let mock = MockServer::start().await;
        mount_corpus(&mock, corpus_hits()).await;
        Mock::given(method("GET"))
            .and(path_regex_works())
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_json(json!({"message": "Slow down"})),
            )
            .mount(&mock)
            .await;
        let mut request = args(CLAIM);
        request.limit = 1;

        let text = server(&mock).cite_while_writing(Parameters(request)).await;

        assert!(text.contains("UNCHECKED (RATE LIMITED 127.0.0.1 for doi:10.5555/demo.a : 429 Too Many Requests: Slow down (Retry-After: 30))"), "{text}");
        assert!(text.contains("candidate(s) above are UNCHECKED"), "{text}");
        let report = crate::http_error::upstream_failure_report(&text).unwrap();
        assert_eq!(report.http_status, Some(429));
        assert_eq!(report.retry_after.as_deref(), Some("30"));
    }

    fn path_regex_works() -> wiremock::matchers::PathRegexMatcher {
        wiremock::matchers::path_regex(r"^/works/")
    }

    #[tokio::test]
    async fn a_429_from_the_corpus_keeps_its_retry_after() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search/enhanced"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "60")
                    .set_body_json(json!({"message": "Daily limit reached"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let text = plain_server(&mock)
            .cite_while_writing(Parameters(args(CLAIM)))
            .await;

        assert!(
            text.starts_with(
                "RATE LIMITED: 429 Too Many Requests: Daily limit reached (Retry-After: 60)"
            ),
            "{text}"
        );
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        crate::server::promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content.unwrap()["retry_after"], "60");
    }

    #[tokio::test]
    async fn a_402_from_the_corpus_keeps_its_retry_after() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search/enhanced"))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("retry-after", "86400")
                    .set_body_json(json!({"message": "Plan required"})),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let text = plain_server(&mock)
            .cite_while_writing(Parameters(args(CLAIM)))
            .await;

        assert_eq!(
            text,
            "PAYMENT REQUIRED: 402 Payment Required: Plan required (Retry-After: 86400)"
        );
        let report = crate::http_error::upstream_failure_report(&text).unwrap();
        assert_eq!(
            (report.kind, report.http_status),
            ("payment_required", Some(402))
        );
        assert_eq!(report.retry_after.as_deref(), Some("86400"));
    }

    #[tokio::test]
    async fn no_hits_is_an_answer_with_a_suggestion() {
        let mock = MockServer::start().await;
        mount_corpus(&mock, json!({"total": 0, "matches": []})).await;

        let text = plain_server(&mock)
            .cite_while_writing(Parameters(args(CLAIM)))
            .await;

        assert!(text.contains("No usable candidates."), "{text}");
        assert!(
            crate::http_error::upstream_failure_report(&text).is_none(),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_oversized_corpus_body_is_refused_while_it_streams() {
        let mock = MockServer::start().await;
        let mut body = corpus_hits();
        body["padding"] = json!("x".repeat(crate::constants::PROVIDER_BODY_LIMIT + 1024));
        mount_corpus(&mock, body).await;

        let text = plain_server(&mock)
            .cite_while_writing(Parameters(args(CLAIM)))
            .await;

        assert!(
            text.starts_with("ERROR corpus : provider response exceeded"),
            "{}",
            &text[..text.len().min(300)]
        );
        assert!(
            text.contains(&crate::constants::PROVIDER_BODY_LIMIT.to_string()),
            "{}",
            &text[..text.len().min(300)]
        );
        assert!(
            !text.contains("Candidates"),
            "no candidate from a refused body"
        );
    }

    #[tokio::test]
    async fn an_unreadable_corpus_body_is_an_error_not_an_empty_answer() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search/enhanced"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"))
            .mount(&mock)
            .await;

        let text = plain_server(&mock)
            .cite_while_writing(Parameters(args(CLAIM)))
            .await;

        assert!(text.starts_with("ERROR corpus : "), "{text}");
        assert!(!text.contains("No usable candidates"), "{text}");
    }

    #[tokio::test]
    async fn a_short_or_wordless_claim_is_refused_before_any_request() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;
        let s = plain_server(&mock);

        for claim in ["too short", "It is what it is, and so it is."] {
            let text = s.cite_while_writing(Parameters(args(claim))).await;
            assert!(text.starts_with("CLIENT ERROR:"), "{claim}: {text}");
        }
        let mut request = args(CLAIM);
        request.limit = 11;
        let text = s.cite_while_writing(Parameters(request)).await;
        assert!(
            text.starts_with("CLIENT ERROR: limit must be between 1 and 10"),
            "{text}"
        );
    }

    #[test]
    fn the_input_schema_is_closed_and_bounded() {
        let tool = Server::new_with_base("http://127.0.0.1:9".into())
            .get_tool("cite_while_writing")
            .expect("tool is registered");
        let schema = Value::Object((*tool.input_schema).clone());
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["claim"]));
        let props = &schema["properties"];
        assert_eq!(
            (
                props["claim"]["minLength"].as_i64(),
                props["claim"]["maxLength"].as_i64()
            ),
            (Some(12), Some(2000))
        );
        assert_eq!(
            (
                props["limit"]["minimum"].as_i64(),
                props["limit"]["maximum"].as_i64()
            ),
            (Some(1), Some(10))
        );
        assert_eq!(props["limit"]["default"], 5);
        assert_eq!(props["check_retractions"]["default"], true);
        assert_eq!(props["year_from"]["minimum"], 1000);
        assert!(serde_json::from_value::<CiteWhileWritingArgs>(
            json!({"claim": CLAIM, "style": "apa"})
        )
        .is_err());
    }

    #[test]
    fn the_readme_transcript_is_the_tool_output() {
        let readme = include_str!("../../../README.md");
        let terms = claim_terms(CLAIM);
        let (ranked, no_doi, too_old) = candidates_from(&corpus_hits(), &terms, None);
        let screened: Vec<(Candidate, Screen)> =
            ranked.into_iter().map(|c| (c, Screen::Clear)).collect();
        let output = render_candidates(CLAIM, &terms, &screened, 2, (no_doi, too_old));
        assert!(
            readme.contains(&output),
            "README transcript for cite_while_writing is stale; it should contain:\n{output}"
        );
    }
}
