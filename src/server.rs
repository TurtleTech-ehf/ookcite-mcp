//! MCP server: tool router, HTTP client, and handlers.

use crate::batch_limits::{
    batch_add_shortfall_line, collect_dois_from_collection_body, format_member_valid_lines,
    format_usage_report, plan_metered_batch, read_only_concurrency, BatchLookupItem,
    DoiResponseCache, MeQuota,
};
use crate::bib_audit::{audit_quota_refusal, parse_bibtex, render_audit, AuditQuota, DoiOutcome};
use crate::bib_export::{self, BibOptions};
use crate::bibliographic::{
    format_validate_doi, format_verify_line, format_verify_line_with_authors,
};
use crate::collection_entries::{
    apply_entry_metadata_overrides, entry_doi, entry_metadata_by_id,
    entry_metadata_overrides_present, format_collection_entry_line, looks_like_doi_token,
    normalize_doi_token, resolve_entry_id_in_collection,
};
use crate::constants::{
    api_base_url, build_api_client, build_provider_client, rate_limit_hint, setup_help_block,
    ANON_BATCH_SOFT_CAP, ANON_DAILY_LOOKUPS, FREE_DAILY_LOOKUPS,
    MIN_CONFIDENT_REVERSE_LOOKUP_SCORE, MUTATE_BATCH_CONCURRENCY, SYNC_BATCH_RESOLVE_LIMIT,
};
use crate::http_error::{
    classify_collection_create_failure, classify_lookup_doi_failure, error_detail, failure_text,
    is_promoted_status, prefixed_failure, severe_lookup_message, upstream_failure_report,
};
use crate::inbound_auth::{self, apply_bearer};
use crate::overrides;
use crate::plaintext::{
    attach_original_query, citation_units_from_parse_payload, collection_entry_metadata,
    detect_bibliography_kind, export_kind, optional_collection_name, render_bibtex_entries,
    split_plaintext_citations, BibliographyKind, ExportKind,
};
use crate::policy::{self, block_mutate};
use crate::resolve_helpers::{
    batch_resolve_request_body, classify_reverse_lookup_response, format_batch_resolve_results,
    format_resolve_candidates, lookup_doi_with_retry, resolve_payload_metadata, resolve_text_body,
    resolver_answer_agrees_with_ranking, reverse_lookup_resolve_body, send_reverse_with_one_retry,
};
use crate::tool_args::*;
use futures::{stream, StreamExt};
use ookcite_mcp::endpoints::{self, Endpoint};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::service::RequestContext;
use rmcp::ServerHandler;
use rmcp::{
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_router, RoleServer,
};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Instant;

mod discovery;

/// Process-wide identity-safe exact-DOI cache (shared across Server clones).
fn shared_doi_cache() -> &'static DoiResponseCache {
    static CACHE: OnceLock<DoiResponseCache> = OnceLock::new();
    CACHE.get_or_init(DoiResponseCache::with_default_ttl)
}

/// BibTeX for one DOI, rendered from the provider-merged metadata already
/// in hand. Uses the same `bib_export::render_pairs` path collection export
/// uses, so a single DOI and a collection export produce the same shape.
///
/// The record the merge returned may not carry `doi` (an OokCite record
/// that was built before the provider overlay, or a publisher record
/// where the registrar field was omitted). The caller asked for a
/// specific DOI, so that DOI is seeded onto the record before render so
/// the resulting entry always carries `doi = {…}`. Same treatment the
/// RIS path already gives it.
fn render_single_bibtex(doi: &str, meta: &serde_json::Value) -> String {
    let mut seeded = meta.clone();
    if let Some(object) = seeded.as_object_mut() {
        let already = object
            .get("doi")
            .and_then(|value| value.as_str())
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .is_some();
        if !already {
            object.insert(
                "doi".to_string(),
                serde_json::Value::String(doi.to_string()),
            );
        }
    }
    let pairs = vec![(seeded, None)];
    match bib_export::render_pairs(&pairs, &BibOptions::default()) {
        Ok(text) => {
            if text.trim().is_empty() {
                format!("BibTeX unavailable for doi:{doi}: the record is empty.")
            } else {
                text
            }
        }
        Err(error) => format!("BibTeX render failed for doi:{doi}: {error}"),
    }
}

/// True when `meta` has at least one author with a non-empty family name.
///
/// The authorless "(in your collection)" stub has no authors. A record the
/// publisher answered for has them, so this is how a member line decides
/// whether to use the full-record formatter.
fn is_publisher_backed(meta: &serde_json::Value) -> bool {
    meta.get("authors")
        .and_then(|value| value.as_array())
        .is_some_and(|authors| {
            authors.iter().any(|author| {
                author
                    .get("family")
                    .and_then(|value| value.as_str())
                    .is_some_and(|family| !family.trim().is_empty())
            })
        })
}

/// Mark a ranked list as below the confidence threshold without erasing it.
///
/// The old code replaced a weak hit with "No confident matches found",
/// which left the agent with nothing to pick between. The near misses are
/// what the caller would disambiguate, so they are kept with a header that
/// names the threshold and the top rank-score.
fn low_confidence_header(ranked: &str) -> String {
    let top = ranked
        .lines()
        .find(|line| line.trim_start().starts_with("1. "))
        .and_then(|line| {
            let open = line.find("[confidence:")?;
            let rest = &line[open + "[confidence:".len()..];
            let close = rest.find(']')?;
            Some(rest[..close].to_string())
        })
        .unwrap_or_else(|| "0".into());
    format!(
        "LOW CONFIDENCE (top rank-score {top} below threshold {:.0}; disambiguate from the list, do not cite without verifying)\n{ranked}",
        MIN_CONFIDENT_REVERSE_LOOKUP_SCORE
    )
}

#[derive(Clone)]
struct ProviderBases {
    crossref: String,
    datacite: String,
    zenodo: String,
    handles: String,
}

impl ProviderBases {
    fn production() -> Self {
        Self {
            crossref: "https://api.crossref.org".into(),
            datacite: "https://api.datacite.org".into(),
            zenodo: "https://zenodo.org/api".into(),
            handles: "https://doi.org/api".into(),
        }
    }

    #[cfg(any(test, feature = "demo-mock"))]
    fn shared(base: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        Self {
            crossref: base.clone(),
            datacite: base.clone(),
            zenodo: base.clone(),
            handles: base,
        }
    }

    fn hosts(&self) -> Vec<String> {
        [&self.crossref, &self.datacite, &self.zenodo, &self.handles]
            .into_iter()
            .filter_map(|base| reqwest::Url::parse(base).ok())
            .filter_map(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
            .collect()
    }

    #[cfg(test)]
    fn test_origins(&self) -> Vec<crate::constants::TestProviderOrigin> {
        [&self.crossref, &self.datacite, &self.zenodo, &self.handles]
            .into_iter()
            .map(|base| crate::constants::TestProviderOrigin::parse(base))
            .collect()
    }
}

fn provider_client(bases: Option<&ProviderBases>, timeout_secs: u64) -> reqwest::Client {
    let hosts = bases.map(ProviderBases::hosts).unwrap_or_default();
    build_provider_client(timeout_secs, hosts)
}

enum ProviderRead {
    TooLarge,
    Redirect,
    /// The request or the body read failed in transport. Holds the cause.
    Failed(String),
    /// A non-success answer. It keeps its status and `Retry-After`.
    Status(discovery::ProviderStatus),
}

struct AuditAccess {
    has_key: bool,
    remaining: Option<u32>,
    limit: Option<u32>,
    plan: String,
    /// `/me` returned 2xx, or there is no key to check.
    checked: bool,
    rejected: Option<String>,
}

impl AuditAccess {
    fn anonymous() -> Self {
        Self {
            has_key: false,
            remaining: None,
            limit: None,
            plan: String::new(),
            checked: true,
            rejected: None,
        }
    }

    fn unchecked() -> Self {
        Self {
            has_key: true,
            remaining: None,
            limit: None,
            plan: String::new(),
            checked: false,
            rejected: None,
        }
    }
}

fn provider_failure(doi: &str, error: ProviderRead) -> String {
    match error {
        ProviderRead::TooLarge => format!(
            "ERROR {doi} : provider response exceeded {} bytes",
            crate::constants::PROVIDER_BODY_LIMIT
        ),
        ProviderRead::Redirect => format!("ERROR {doi} : provider redirect refused"),
        ProviderRead::Failed(cause) => format!("UNREACHABLE {doi} : {cause}"),
        ProviderRead::Status(status) => status.text(&format!("{} for doi:{doi}", status.host())),
    }
}

/// doi.org's handle API answers `responseCode` 1 for a registered handle and
/// 100 for one that does not exist. Anything else, including a body that is
/// not JSON, a missing or non-numeric code, or another code (2 is a server
/// error, 200 is "values not found"), says nothing about the DOI. It is an
/// error, not a verdict, so it can never become "not registered".
fn handle_verdict(bytes: &[u8]) -> Result<bool, String> {
    let body: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "the body is not JSON".to_string())?;
    let code = match body.get("responseCode") {
        Some(serde_json::Value::Number(number)) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.fract() == 0.0)
                .map(|value| value as i64)
        }),
        Some(_) => None,
        None => return Err("it has no responseCode".to_string()),
    };
    match code {
        Some(1) => Ok(true),
        Some(100) => Ok(false),
        Some(other) => Err(format!("responseCode {other}")),
        None => Err("responseCode is not a number".to_string()),
    }
}

fn provider_send_error(doi: &str, error: &reqwest::Error) -> String {
    provider_failure(doi, provider_read_error(error))
}

fn provider_read_error(error: &reqwest::Error) -> ProviderRead {
    if error.is_redirect() || error.to_string().contains("provider redirect refused") {
        ProviderRead::Redirect
    } else {
        ProviderRead::Failed(transport_cause(error))
    }
}

/// Why a provider request failed, without the URL: what happened, to which
/// host, and the underlying causes. A timeout, a refused connection and a
/// DNS failure read differently, and the caller needs to know which.
pub(crate) fn transport_cause(error: &reqwest::Error) -> String {
    let host = error
        .url()
        .and_then(|url| url.host_str())
        .unwrap_or("the provider");
    let what = if error.is_timeout() {
        format!("timed out reading {host}")
    } else if error.is_connect() {
        format!("could not connect to {host}")
    } else if error.is_body() || error.is_decode() {
        format!("the response from {host} ended early")
    } else {
        format!("the request to {host} failed")
    };
    let mut causes: Vec<String> = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        let text = inner.to_string();
        if !text.is_empty() && !causes.contains(&text) {
            causes.push(text);
        }
        source = inner.source();
    }
    let detail: String = causes.join(": ").chars().take(160).collect();
    if detail.is_empty() {
        what
    } else {
        format!("{what} ({detail})")
    }
}

/// Read a provider body until it ends or it passes [`PROVIDER_BODY_LIMIT`].
async fn read_capped_body(mut response: reqwest::Response) -> Result<Vec<u8>, ProviderRead> {
    let mut buf = Vec::new();
    loop {
        let next = response
            .chunk()
            .await
            .map_err(|error| provider_read_error(&error))?;
        let Some(chunk) = next else {
            return Ok(buf);
        };
        if buf.len().saturating_add(chunk.len()) > crate::constants::PROVIDER_BODY_LIMIT {
            return Err(ProviderRead::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
}

pub(crate) async fn read_capped_api_body(
    response: reqwest::Response,
    source: &str,
) -> Result<Vec<u8>, String> {
    read_capped_body(response)
        .await
        .map_err(|error| provider_failure(source, error))
}

pub(crate) struct DoiLookup {
    pub metadata: serde_json::Value,
    pub retrieved_at: Instant,
    pub from_cache: bool,
    /// Where the record came from.
    ///
    /// `Corpus` is a hit on `/api/v1/lookup/doi`. `NotInCorpus` is a 404 on
    /// that lookup paired with a publisher record: the DOI is registered
    /// and the registrar answered, but OokCite's corpus did not. The verify
    /// head used to print INVALID for that case; the answer is NOT-IN-CORPUS
    /// because the paper exists and the fields below are the publisher's.
    pub source: DoiSource,
    /// The raw OokCite corpus response for this DOI, before provider fields
    /// were merged in. `None` when the corpus had no entry (NotInCorpus).
    /// Used to compute per-field provenance for the structured work record.
    pub stored_before_merge: Option<serde_json::Value>,
    /// The raw provider record (`{"crossref": ..}`, `{"datacite": ..}` or
    /// `{"zenodo": ..}`) that was merged into the metadata, when any.
    pub provider_record: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoiSource {
    Corpus,
    NotInCorpus,
}

impl DoiLookup {
    fn freshness_line(&self) -> String {
        let age_secs = self.retrieved_at.elapsed().as_secs();
        let source = if self.from_cache { "cached" } else { "live" };
        let origin = match self.source {
            DoiSource::Corpus => "ookcite corpus",
            DoiSource::NotInCorpus => {
                "publisher record (not in OokCite corpus; Crossref or DataCite)"
            }
        };
        format!("Metadata: {source} (retrieved {age_secs}s ago) from {origin}")
    }
}

#[derive(Clone)]
pub struct Server {
    tool_router: ToolRouter<Self>,
    pub(crate) http: reqwest::Client,
    /// doi.org, Crossref, DataCite, and Zenodo. Redirects stay on their hosts.
    provider_http: reqwest::Client,
    /// Unpaywall and OpenAlex, for the discovery tools. Absent in unit
    /// tests until [`Server::with_discovery`] points them at a mock.
    discovery_http: reqwest::Client,
    discovery: Option<discovery::DiscoveryBases>,
    api_base: String,
    doi_cache: DoiResponseCache,
    /// Crossref, DataCite, Zenodo, and doi.org. Absent in unit tests so they
    /// stay on the mocked OokCite API and never call a live provider.
    providers: Option<ProviderBases>,
    /// Local override store, decided once by the entry point. Off unless set.
    overrides: overrides::OverrideStore,
    /// Whether this process serves many accounts over HTTP, decided once by
    /// the entry point. Off unless set.
    shared_http: bool,
}

#[tool_router]
impl Server {
    pub fn new() -> Self {
        // The bearer is attached per request. A default header would leak a
        // process key onto HTTP calls that did not present one.
        let providers = ProviderBases::production();
        let discovery = discovery::DiscoveryBases::production();
        Self {
            tool_router: Self::tool_router()
                + Self::ridley_tool_router()
                + discovery::discovery_routers(),
            http: build_api_client(30, reqwest::header::HeaderMap::new()),
            provider_http: provider_client(Some(&providers), 30),
            discovery_http: discovery::discovery_client(Some(&discovery), 30),
            discovery: Some(discovery),
            api_base: api_base_url(),
            doi_cache: shared_doi_cache().clone(),
            providers: Some(providers),
            overrides: overrides::OverrideStore::Disabled,
            shared_http: false,
        }
    }

    /// Use the override store the entry point chose with
    /// [`overrides::OverrideStore::for_transport`].
    pub fn with_overrides(mut self, store: overrides::OverrideStore) -> Self {
        self.overrides = store;
        self
    }

    /// Declare that this process serves many accounts over HTTP.
    pub fn with_shared_http(mut self, shared: bool) -> Self {
        self.shared_http = shared;
        self
    }

    pub(crate) fn is_shared_http(&self) -> bool {
        self.shared_http
    }

    /// Run one tool call under the credential its request carried.
    ///
    /// `inbound` is `Some` when the request arrived over HTTP with its
    /// headers attached, and holds whatever bearer those headers carried,
    /// which may be absent. A shared HTTP server has no process credential
    /// to lend a request that arrives without them: falling through to
    /// `OOKCITE_API_KEY` would spend and read the operator's own account on
    /// behalf of whoever sent the request. Only a single-user process, which
    /// is the person that key belongs to, falls through to it.
    async fn in_credential_scope<T>(
        &self,
        inbound: Option<Option<String>>,
        call: impl std::future::Future<Output = T>,
    ) -> T {
        match inbound {
            Some(key) => inbound_auth::with_http_bearer(key, call).await,
            None if self.shared_http => inbound_auth::with_http_bearer(None, call).await,
            None => call.await,
        }
    }

    async fn fetch_me_quota(&self) -> Option<MeQuota> {
        if !inbound_auth::has_api_key() {
            return None;
        }
        let resp = self
            .send_read(|| self.request(endpoints::ME, &[]))
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        Some(MeQuota::from_json(&v))
    }

    /// Load DOIs from all collections (bounded: first page of list + each GET).
    /// Used for upfront membership so collection-local DOIs skip metered fan-out.
    async fn load_collection_doi_membership(&self) -> (HashSet<String>, HashMap<String, String>) {
        let mut dois = HashSet::new();
        let mut titles = HashMap::new();
        if !inbound_auth::has_api_key() {
            return (dois, titles);
        }
        let Ok(resp) = crate::resolve_helpers::send_idempotent_with_retry(|| {
            self.request(endpoints::COLLECTIONS_LIST, &[])
        })
        .await
        else {
            return (dois, titles);
        };
        if !resp.status().is_success() {
            return (dois, titles);
        }
        let list: serde_json::Value = resp.json().await.unwrap_or_default();
        let cols = list
            .as_array()
            .cloned()
            .or_else(|| list["collections"].as_array().cloned())
            .unwrap_or_default();
        // Cap collections scanned so preflight stays cheaper than the batch.
        for col in cols.into_iter().take(32) {
            let id = col["id"].as_str().unwrap_or("");
            if id.is_empty() {
                continue;
            }
            let Ok(cr) = self
                .send_read(|| self.request(endpoints::COLLECTION_GET, &[("id", id)]))
                .await
            else {
                continue;
            };
            if !cr.status().is_success() {
                continue;
            }
            let body: serde_json::Value = cr.json().await.unwrap_or_default();
            collect_dois_from_collection_body(&body, &mut dois, &mut titles);
        }
        (dois, titles)
    }

    async fn lookup_doi_with_freshness(&self, doi: &str) -> Result<DoiLookup, String> {
        // The cache is one map per process, so over HTTP it would hand one
        // account's lookup to the next request and no one would be metered
        // for it. Only a single-user process may read or write it.
        let cacheable = !inbound_auth::in_http_scope();
        if cacheable {
            if let Some(cached) = self.doi_cache.get_valid_with_timing(doi) {
                return Ok(DoiLookup {
                    metadata: cached.metadata,
                    retrieved_at: cached.retrieved_at,
                    from_cache: true,
                    source: DoiSource::Corpus,
                    // The pre-merge stored row and the raw provider
                    // record cached on the first fetch are returned on
                    // the hit, so the structured record built here has
                    // the same per-field provenance, confidence, and
                    // consistency warnings a fresh lookup would have
                    // produced. The first version of this cache dropped
                    // both, which left a cache hit without ORCID,
                    // abstract, license, retraction, or the "live
                    // provider agrees with OokCite" provenance tag.
                    stored_before_merge: cached.stored_before_merge,
                    provider_record: cached.provider_record,
                });
            }
        }
        self.require_registered(doi).await?;
        let r = lookup_doi_with_retry(&self.http, &self.api_base, doi).await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let raw: serde_json::Value = resp.json().await.unwrap_or_default();
                let mut meta = raw.clone();
                let mut provider_record = None;
                if self.providers.is_some() {
                    match self.provider_record(doi).await {
                        Ok(Some(record)) => {
                            meta = bib_export::merge_provider_metadata(&meta, &record);
                            provider_record = Some(record);
                        }
                        Ok(None) => {}
                        // The registration check already failed closed. A provider
                        // miss still leaves the OokCite record usable for verify.
                        Err(_) => {}
                    }
                }
                let retrieved_at = if cacheable {
                    self.doi_cache
                        .put_full_if_identity_ok(
                            doi,
                            meta.clone(),
                            Some(raw.clone()),
                            provider_record.clone(),
                        )
                        .unwrap_or_else(Instant::now)
                } else {
                    Instant::now()
                };
                Ok(DoiLookup {
                    metadata: meta,
                    retrieved_at,
                    from_cache: false,
                    source: DoiSource::Corpus,
                    stored_before_merge: Some(raw),
                    provider_record,
                })
            }
            Ok(resp) if resp.status().as_u16() == 404 => {
                // Registered DOI (require_registered passed) but the OokCite
                // corpus has no entry. The old path reported INVALID and
                // stopped, which is where agents learned to bypass this
                // server for the registrar record they already have. Try the
                // publisher directly and return a NotInCorpus lookup when
                // Crossref or DataCite has it; otherwise classify as before.
                if self.providers.is_none() {
                    return Err(classify_lookup_doi_failure(resp, doi).await);
                }
                match self.provider_record(doi).await {
                    Ok(Some(record)) => {
                        let meta = bib_export::merge_provider_metadata(
                            &serde_json::json!({ "doi": doi }),
                            &record,
                        );
                        Ok(DoiLookup {
                            metadata: meta,
                            retrieved_at: Instant::now(),
                            from_cache: false,
                            source: DoiSource::NotInCorpus,
                            stored_before_merge: None,
                            provider_record: Some(record),
                        })
                    }
                    Ok(None) => Err(classify_lookup_doi_failure(resp, doi).await),
                    // A provider throttle, timeout, or payment wall must
                    // reach the caller: it is not a miss and it is not an
                    // invalid DOI.
                    Err(error) => Err(error),
                }
            }
            Ok(resp) => Err(classify_lookup_doi_failure(resp, doi).await),
            Err(e) => Err(format!("ERROR {doi} : {e}")),
        }
    }

    async fn lookup_doi_json_cached(&self, doi: &str) -> Result<serde_json::Value, String> {
        self.lookup_doi_with_freshness(doi)
            .await
            .map(|lookup| lookup.metadata)
    }

    /// Registration, one OokCite lookup, and one publisher record.
    ///
    /// A 404 from OokCite is not by itself a broken DOI: the publisher record
    /// can still name the work and its retraction. A rate limit or a restart
    /// stops the entry. The process cache is not used, so a retraction read
    /// for this audit is not a metadata copy from an earlier tool call.
    async fn inspect_doi(&self, doi: &str) -> DoiOutcome {
        if self.providers.is_some() {
            match self.registration_status(doi).await {
                Ok(false) => {
                    return DoiOutcome::Broken(crate::bib_audit::BROKEN_NOT_REGISTERED.into());
                }
                // doi.org gave no answer about this DOI: a throttle, a refusal
                // (400, 401, 403, 409, 422), a body past the cap, a refused
                // redirect, or a body that is not a handle answer. The entry
                // was not checked, and the caller must see a failed check
                // with the status, not a clean-looking row.
                Err(error) => return DoiOutcome::Upstream(error),
                Ok(true) => {}
            }
        }
        let lookup = lookup_doi_with_retry(&self.http, &self.api_base, doi).await;
        let metadata = match lookup {
            Ok(resp) if resp.status().is_success() => {
                resp.json::<serde_json::Value>().await.unwrap_or_default()
            }
            Ok(resp) => {
                let failure = classify_lookup_doi_failure(resp, doi).await;
                if upstream_failure_report(&failure).is_some() {
                    return DoiOutcome::Upstream(failure);
                }
                serde_json::Value::Null
            }
            Err(error) => return DoiOutcome::Upstream(format!("ERROR {doi} : {error}")),
        };
        let (provider, provider_error) = match self.provider_record(doi).await {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        };
        if !metadata.is_object() && provider.is_none() {
            if let Some(error) = provider_error {
                return DoiOutcome::Upstream(error);
            }
            return DoiOutcome::Broken(crate::bib_audit::BROKEN_NO_RECORD.into());
        }
        let metadata = match &provider {
            Some(record) if metadata.is_object() => {
                bib_export::merge_provider_metadata(&metadata, record)
            }
            Some(record) => bib_export::merge_provider_metadata(&serde_json::json!({}), record),
            None => metadata,
        };
        DoiOutcome::Resolved {
            metadata,
            provider,
            provider_error,
        }
    }

    /// Ok when doi.org's handle answer says the DOI is registered
    /// (`responseCode` 1). `UNREGISTERED` only for `responseCode` 100. Any
    /// other answer, such as a refusal, a throttle, or a body that is not a
    /// handle verdict, is the error `registration_status` built for it.
    async fn require_registered(&self, doi: &str) -> Result<(), String> {
        if self.providers.is_none() {
            return Ok(());
        }
        match self.registration_status(doi).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "UNREGISTERED {doi}: the DOI is not registered at doi.org"
            )),
            Err(error) => Err(error),
        }
    }

    async fn registration_status(&self, doi: &str) -> Result<bool, String> {
        let Some(bases) = &self.providers else {
            return Ok(true);
        };
        let url = format!(
            "{}/handles/{}",
            bases.handles.trim_end_matches('/'),
            encode_path_segment(doi)
        );
        let response = self
            .provider_request(&url)
            .send()
            .await
            .map_err(|error| provider_send_error(doi, &error))?;
        let status = response.status();
        if !status.is_success() && status.as_u16() != 404 {
            // Only a 404 is doi.org saying the handle does not exist. A
            // throttle, a restart, or a refusal (400, 401, 403, 409, 422) says
            // nothing about the DOI, and reading its body for a responseCode
            // called every such DOI unregistered.
            return Err(provider_failure(
                doi,
                ProviderRead::Status(discovery::read_provider_status(response).await),
            ));
        }
        let host = response.url().host_str().unwrap_or("doi.org").to_string();
        let bytes = read_capped_body(response)
            .await
            .map_err(|error| provider_failure(doi, error))?;
        handle_verdict(&bytes).map_err(|why| {
            format!(
                "ERROR {host} for doi:{doi} : the handle answer ({status}) is not a registration verdict: {why}"
            )
        })
    }

    /// True when the server has a provider base (Crossref/DataCite/handles)
    /// configured. The open-access fallback uses this before reading a
    /// publisher record so a test server without providers does not try.
    pub(crate) fn providers_available(&self) -> bool {
        self.providers.is_some()
    }

    /// Fetch a publisher record for `doi` without the registration check.
    ///
    /// Used by the open-access fallback: a session without a contact email
    /// cannot call Unpaywall, but it can still read Crossref's `license[]`
    /// and the landing URL (or the DataCite rights list) and tell the
    /// caller what the publisher recorded. A doi.org throttle or restart
    /// must not keep the record from reaching the caller, so this takes
    /// the direct provider_record path instead of require_registered.
    pub(crate) async fn license_record_for_doi(
        &self,
        doi: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        self.provider_record(doi).await
    }

    async fn provider_record(&self, doi: &str) -> Result<Option<serde_json::Value>, String> {
        let Some(bases) = &self.providers else {
            return Ok(None);
        };
        let enc = encode_path_segment(doi);
        let crossref_url = format!("{}/works/{enc}", bases.crossref.trim_end_matches('/'));
        let datacite_url = format!("{}/dois/{enc}", bases.datacite.trim_end_matches('/'));
        let mut record = match self.provider_json(&crossref_url).await {
            Ok(Some(body)) => {
                let message = body
                    .get("message")
                    .cloned()
                    .filter(|value| value.is_object())
                    .unwrap_or(body);
                Some(serde_json::json!({ "crossref": message }))
            }
            Ok(None) => match self.provider_json(&datacite_url).await {
                Ok(Some(body)) => {
                    let attrs = body
                        .pointer("/data/attributes")
                        .cloned()
                        .filter(|value| value.is_object())
                        .unwrap_or(body);
                    Some(serde_json::json!({ "datacite": attrs }))
                }
                Ok(None) => None,
                Err(error) => return Err(provider_failure(doi, error)),
            },
            Err(error) => return Err(provider_failure(doi, error)),
        };
        if let Some(record) = record.as_mut() {
            if doi.to_ascii_lowercase().starts_with("10.5281/zenodo.") {
                match self.zenodo_record(doi).await {
                    Ok(Some(zenodo)) => {
                        record
                            .as_object_mut()
                            .expect("provider record is an object")
                            .insert("zenodo".into(), zenodo);
                    }
                    Ok(None) => {}
                    Err(error) => return Err(provider_failure(doi, error)),
                }
            }
        }
        Ok(record)
    }

    async fn zenodo_record(&self, doi: &str) -> Result<Option<serde_json::Value>, ProviderRead> {
        let lower = doi.to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix("10.5281/zenodo.") else {
            return Ok(None);
        };
        let id = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if id.is_empty() {
            return Ok(None);
        }
        let Some(bases) = self.providers.as_ref() else {
            return Ok(None);
        };
        let url = format!("{}/records/{id}", bases.zenodo.trim_end_matches('/'));
        let Some(body) = self.provider_json(&url).await? else {
            return Ok(None);
        };
        let version = body
            .pointer("/metadata/version")
            .cloned()
            .or_else(|| body.get("version").cloned());
        Ok(Some(serde_json::json!({
            "doi": body.get("doi").cloned().unwrap_or(serde_json::Value::Null),
            "conceptdoi": body.get("conceptdoi").cloned().unwrap_or(serde_json::Value::Null),
            "version": version.unwrap_or(serde_json::Value::Null),
        })))
    }

    fn provider_request(&self, url: &str) -> reqwest::RequestBuilder {
        self.provider_http
            .get(url)
            .header(
                reqwest::header::USER_AGENT,
                discovery::provider_user_agent(),
            )
            .header(reqwest::header::ACCEPT, "application/json")
    }

    async fn provider_json(&self, url: &str) -> Result<Option<serde_json::Value>, ProviderRead> {
        let response = self
            .provider_request(url)
            .send()
            .await
            .map_err(|error| provider_read_error(&error))?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            // A throttle or a payment wall names itself and keeps its
            // Retry-After. Folding it into a transport failure told the
            // caller nothing about when to try again.
            return Err(ProviderRead::Status(
                discovery::read_provider_status(response).await,
            ));
        }
        let bytes = read_capped_body(response).await?;
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(body) if !body.is_null() => Ok(Some(body)),
            _ => Ok(None),
        }
    }

    async fn finish_reverse_hit(
        &self,
        hit: crate::resolve_helpers::ReverseLookupMatch,
    ) -> Result<String, String> {
        if let Some(doi) = hit
            .top_doi
            .as_deref()
            .filter(|doi| looks_like_doi_token(doi))
        {
            self.require_registered(doi).await?;
        }
        Ok(hit.output)
    }

    async fn render_collection_bib(
        &self,
        entries: &[serde_json::Value],
        options: BibOptions,
    ) -> String {
        let mut pairs = Vec::with_capacity(entries.len());
        let mut errors = Vec::new();
        for entry in entries {
            let stored = collection_entry_metadata(entry);
            let doi = stored
                .get("doi")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|doi| !doi.is_empty())
                .map(str::to_string);
            let has_source = crate::bib_source::source_from_meta(&stored).is_some();
            if self.providers.is_none() || (has_source && !options.enrich) {
                pairs.push((stored, None));
                continue;
            }
            let Some(doi) = doi else {
                pairs.push((stored, None));
                continue;
            };
            if let Err(error) = self.require_registered(&doi).await {
                errors.push(error);
                continue;
            }
            match self.provider_record(&doi).await {
                Ok(record) => pairs.push((stored, record)),
                Err(error) => errors.push(error),
            }
        }
        if !errors.is_empty() {
            return errors.join("\n");
        }
        match bib_export::render_pairs(&pairs, &options) {
            Ok(bib) => bib,
            Err(error) => error.to_string(),
        }
    }

    /// Collection-local fallback when no publisher record can be fetched.
    /// Never used on its own by `batch_format` or `verify_references`: both
    /// go through `member_metadata`, which asks the publisher first so a
    /// stored DOI is not formatted as an authorless "(in your collection)"
    /// stub when Crossref or DataCite still hold the work. The stub stays
    /// as the offline / provider-unavailable degradation.
    fn synthetic_member_metadata(doi: &str, title: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "doi": doi,
            "title": title.unwrap_or("(in your collection)"),
            "entry_type": "article",
            "authors": [],
        })
    }

    /// Sentinel key attached to a member stub when the publisher read
    /// failed (429, 5xx, timeout, transport). `verify_references` sees
    /// it and prints DEGRADED on the member line so the caller does not
    /// mistake an authorless stub for a verified publisher record.
    pub(crate) const MEMBER_DEGRADED_KEY: &str = "_ookcite_member_degraded";

    /// Build the member→metadata map used by batch tools, in input order.
    ///
    /// Runs the per-member provider reads with the shared read concurrency,
    /// so a large collection batch does not fan out one request at a time.
    async fn member_entries_for(
        &self,
        members: &[(String, Option<String>)],
    ) -> HashMap<String, serde_json::Value> {
        if members.is_empty() {
            return HashMap::new();
        }
        let conc = read_only_concurrency();
        let futs = members.iter().cloned().map(|(doi, title)| {
            let server = self.clone();
            async move {
                let meta = server.member_metadata(&doi, title.as_deref()).await;
                (doi, meta)
            }
        });
        stream::iter(futs)
            .buffered(conc)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }

    /// Metadata for a DOI the caller already keeps in a collection.
    ///
    /// Collection membership is the free path: it skips the OokCite lookup
    /// meter. The previous policy also skipped the publisher, which left
    /// `batch_format` and `verify_references` formatting an authorless
    /// "(in your collection)" stub -- a line that said VALID without
    /// checking the record. This fetches Crossref or DataCite directly
    /// (same path the fall-through and audit tools take), merges those
    /// fields, and returns the full record. No metered OokCite call is
    /// made. A provider failure degrades to the stub so an offline run
    /// still formats something rather than erroring out.
    async fn member_metadata(&self, doi: &str, title: Option<&str>) -> serde_json::Value {
        if self.providers.is_none() {
            // No provider base configured (unit tests, offline run):
            // the stub is the final answer, not a publisher failure, so
            // the degraded marker is not attached.
            return Self::synthetic_member_metadata(doi, title);
        }
        match self.provider_record(doi).await {
            Ok(Some(record)) => {
                let seed = serde_json::json!({ "doi": doi });
                bib_export::merge_provider_metadata(&seed, &record)
            }
            // A clean 404 from both Crossref and DataCite: the collection
            // DOI exists (the user has it) but no publisher record is
            // reachable. The stub is honest here.
            Ok(None) => Self::synthetic_member_metadata(doi, title),
            // A 429, a 5xx, a timeout, or a transport failure: the
            // record probably exists but we could not read it. Mark the
            // stub degraded so verify_references prints DEGRADED instead
            // of VALID, and keep the reason on the stub so an agent can
            // retry later.
            Err(reason) => {
                let mut stub = Self::synthetic_member_metadata(doi, title);
                if let Some(object) = stub.as_object_mut() {
                    object.insert(
                        Self::MEMBER_DEGRADED_KEY.to_string(),
                        serde_json::Value::String(reason),
                    );
                }
                stub
            }
        }
    }

    /// Preflight + membership for multi-DOI tools; metered DOIs use cache then API.
    async fn resolve_dois_with_preflight(
        &self,
        dois: &[String],
    ) -> Result<Vec<serde_json::Value>, String> {
        let has_key = inbound_auth::has_api_key();
        let quota = self.fetch_me_quota().await;
        let (member_dois, member_titles) = if has_key {
            self.load_collection_doi_membership().await
        } else {
            (HashSet::new(), HashMap::new())
        };
        let pf = plan_metered_batch(dois, &member_dois, &member_titles, quota.as_ref(), has_key);
        if let Some(msg) = pf.refuse_message {
            return Err(msg);
        }
        let member_entries = self.member_entries_for(&pf.members).await;
        let conc = read_only_concurrency();
        let futs: Vec<_> = pf
            .need_lookup
            .iter()
            .map(|item| {
                let server = self.clone();
                let doi = item.text.clone();
                async move { server.lookup_doi_json_cached(&doi).await }
            })
            .collect();
        // `buffered`, not `buffer_unordered`: `group_cite` numbers these
        // entries by position and `generate_citation_keys` returns one key
        // per position, so completion order is the user's numbering.
        let looked: Vec<_> = stream::iter(futs).buffered(conc).collect::<Vec<_>>().await;
        let mut looked_by_position: HashMap<usize, Result<serde_json::Value, String>> = pf
            .need_lookup
            .iter()
            .map(|item| item.index)
            .zip(looked)
            .collect();
        let mut entries = Vec::new();
        let mut failures = Vec::new();
        for (index, doi) in dois.iter().enumerate() {
            let normalized = normalize_doi_token(doi);
            if let Some(member) = member_entries.get(&normalized) {
                entries.push(member.clone());
                continue;
            }
            match looked_by_position.remove(&index) {
                Some(Ok(meta)) => entries.push(meta),
                Some(Err(error)) => failures.push(error),
                None => {}
            }
        }
        // A throttle or a restart has to stay visible. Dropping it here
        // made group_cite and key generation report a miss, or a grouped
        // marker, for a call the server refused.
        if let Some(message) = severe_lookup_message(&failures) {
            return Err(message);
        }
        Ok(entries)
    }

    /// Sync lines for `ookcite-mcp doctor` before async probes.
    pub fn doctor_report_sync_prelude() -> String {
        policy::policy_summary_lines().join("\n")
    }

    /// Full readiness report (API health + optional /me; never prints full API key).
    pub async fn doctor_report(&self) -> String {
        let mut out = policy::policy_summary_lines();
        out.push(format!("API base: {}", self.api_base));

        let health = self
            .send_read(|| self.request(endpoints::HEALTH, &[]))
            .await;
        match health {
            Ok(resp) if resp.status().is_success() => {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                let status = data["status"].as_str().unwrap_or("unknown");
                let version = data["version"].as_str().unwrap_or("unknown");
                out.push(format!("API health: {status} (server version {version})"));
            }
            Ok(resp) => {
                out.push(format!("API health: HTTP {} (unhealthy)", resp.status()));
                out.push(setup_help_block());
            }
            Err(e) => {
                out.push(format!("API health: unreachable ({e})"));
                out.push(setup_help_block());
            }
        }

        if inbound_auth::has_api_key() {
            match self.send_read(|| self.request(endpoints::ME, &[])).await {
                Ok(resp) if resp.status().is_success() => {
                    let data: serde_json::Value = resp.json().await.unwrap_or_default();
                    let plan = data["plan"].as_str().unwrap_or("?");
                    let user = data["username"].as_str().unwrap_or("?");
                    let rem = data["lookups_remaining"].as_u64();
                    let lim = data["lookups_limit"].as_u64();
                    let quota = match (rem, lim) {
                        (Some(r), Some(l)) => format!("{r}/{l} lookups remaining"),
                        _ => "quota n/a".into(),
                    };
                    // Never include token material from the response body.
                    out.push(format!("Auth /me: ok user={user} plan={plan} ({quota})"));
                }
                Ok(resp) => {
                    out.push(format!(
                        "Auth /me: HTTP {} (key rejected or expired?)",
                        resp.status()
                    ));
                    out.push(setup_help_block());
                }
                Err(e) => out.push(format!("Auth /me: request failed ({e})")),
            }
        } else {
            out.push("Auth /me: skipped (no OOKCITE_API_KEY; anonymous IP limits apply)".into());
        }

        out.push(String::new());
        out.push("Next: health_check for a short probe; doctor after env changes.".into());
        out.push("Never paste full API keys into chat; use redact-friendly doctor output.".into());
        out.join("\n")
    }

    /// Build a request to a registered endpoint, substituting any path
    /// placeholders. Method is taken from the registry, ensuring the call
    /// site cannot drift from the contract.
    fn request(&self, ep: Endpoint, params: &[(&str, &str)]) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.api_base, ep.render(params));
        let builder = match ep.method {
            "GET" => self.http.get(url),
            "POST" => self.http.post(url),
            "PUT" => self.http.put(url),
            "PATCH" => self.http.patch(url),
            "DELETE" => self.http.delete(url),
            other => panic!(
                "ookcite-mcp: unsupported HTTP method `{other}` in registry for {}",
                ep.path
            ),
        };
        apply_bearer(builder)
    }

    /// A read, or a write the collection store treats as a duplicate.
    /// Three attempts, `Retry-After` capped at 5s. One 503 during a
    /// restart is not an answer.
    async fn send_read<F>(&self, make: F) -> Result<reqwest::Response, reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        crate::resolve_helpers::send_idempotent_with_retry(make).await
    }

    #[tool(
        name = "search_styles",
        description = "Search for available CSL citation styles by name. Returns a list of matching style IDs to use in formatting tools. Call this when the user names a style (\"IEEE\", \"Chicago\", a journal's house style) and you need its exact ID before calling format_citation or batch_format.",
        annotations(
            title = "Search CSL styles",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn search_styles(&self, Parameters(args): Parameters<StyleSearchArgs>) -> String {
        let r = self
            .send_read(|| {
                self.request(endpoints::STYLES_SEARCH, &[])
                    .query(&[("q", args.query.as_str())])
            })
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let styles: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                let mut out = Vec::new();
                for s in styles.iter().take(15) {
                    let id = s["id"].as_str().unwrap_or("?");
                    let title = s["title"].as_str().unwrap_or("?");
                    out.push(format!("ID: {id} | Title: {title}"));
                }
                if out.is_empty() {
                    "No styles found".into()
                } else {
                    out.join("\n")
                }
            }
            Ok(resp) => failure_text(resp, "Style search failed").await,
            Err(error) => format!("Style search failed: {error}"),
        }
    }

    #[tool(
        name = "validate_doi",
        description = "First call for any DOI. Checks the DOI is registered at doi.org, reads the full record (title, authors with ORCID when the provider carries it, year, container/journal, volume, issue, pages, publisher, URL, abstract, references, license, type, retraction/update-to, open-access PDF URL when known), and compares it with whichever claim fields you pass. Call this before you ask Crossref, OpenAlex, DataCite or Semantic Scholar directly: when the OokCite corpus holds the DOI you get the corpus record overlaid with the provider's; when the corpus does not, OokCite falls through to Crossref (then DataCite, then Zenodo) and returns the publisher record itself. The content block is a human-readable VALID/NOT-IN-CORPUS/MISMATCH/INCOMPLETE/INVALID summary; structured_content is the full ookcite.work_record/v1 (schema advertised on the tool listing). Every field in it is a {value, provenance} pair — provenance is ookcite_corpus, ookcite_corpus+<provider> when both sides agree, crossref, datacite, zenodo, or unknown — plus a coarse confidence (high/medium/low) and the metadata age in seconds. OokCite-first on a corpus hit: a corpus title or author list wins over a disagreeing provider's; the record carries both sources in the provenance so you can see they disagreed. VALID means every claim field you passed agrees with the record. MISMATCH is a disagreement. INCOMPLETE is a claim field the record does not carry. NOT-IN-CORPUS is a corpus miss answered by the publisher. INVALID is a DOI the registrar does not know. Omitting the claim only checks the DOI exists, which does not prove it is the paper you named. Prefer verify_references for several DOIs.",
        annotations(title = "Validate DOI", read_only_hint = true, idempotent_hint = true),
        output_schema = crate::work_record::output_schema()
    )]
    async fn validate_doi(&self, Parameters(args): Parameters<DoiArgs>) -> String {
        match self.lookup_doi_with_freshness(&args.doi).await {
            Ok(lookup) => {
                let body = format_validate_doi(&args.doi, &lookup.metadata, &args.claim);
                // NOT-IN-CORPUS replaces a plain VALID head when the record
                // came from the publisher fall-through. The old head hid
                // which store answered, which made every corpus gap look
                // like a validation pass without context.
                let body = if lookup.source == DoiSource::NotInCorpus && body.starts_with("VALID\n")
                {
                    body.replacen("VALID\n", "NOT-IN-CORPUS\n", 1)
                } else {
                    body
                };
                let summary = format!("{body}\n{}", lookup.freshness_line());
                let claim_status = if args.claim.is_empty() {
                    None
                } else {
                    Some(crate::bibliographic::claim_verdict(&lookup.metadata, &args.claim).status)
                };
                let record = crate::work_record::build_validate_doi_record(
                    &args.doi,
                    lookup.source,
                    lookup.stored_before_merge.as_ref(),
                    lookup.provider_record.as_ref(),
                    &lookup.metadata,
                    lookup.retrieved_at.elapsed().as_secs(),
                    lookup.from_cache,
                    &summary,
                    claim_status,
                );
                crate::work_record::append_structured_marker(&summary, &record)
            }
            Err(msg) => msg,
        }
    }

    #[tool(
        name = "lookup_isbn",
        description = "Look up a book by ISBN. Returns title, authors, publisher, year, and pages. Call this for book references; use validate_doi instead for journal articles and anything else with a DOI.",
        annotations(title = "Lookup ISBN", read_only_hint = true, idempotent_hint = true)
    )]
    async fn lookup_isbn(&self, Parameters(args): Parameters<IsbnArgs>) -> String {
        let r = self
            .send_read(|| {
                self.request(endpoints::LOOKUP_ISBN, &[])
                    .json(&serde_json::json!({ "isbn": args.isbn }))
            })
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let meta: serde_json::Value = resp.json().await.unwrap_or_default();
                let title = meta["title"].as_str().unwrap_or("?");
                let authors = meta["authors"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x["family"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let year = meta["date"]["year"]
                    .as_i64()
                    .map(|y| y.to_string())
                    .unwrap_or_default();
                let publisher = meta["publisher"].as_str().unwrap_or("N/A");
                let pages = meta["pages"].as_str().unwrap_or("N/A");
                format!(
                    "VALID\nISBN: {}\nTitle: {title}\nAuthors: {authors}\nYear: {year}\nPublisher: {publisher}\nPages: {pages}",
                    args.isbn
                )
            }
            Ok(r) if r.status().as_u16() == 429 => {
                format!(
                    "RATE LIMITED: {}\n{}",
                    error_detail(r).await,
                    rate_limit_hint()
                )
            }
            Ok(r) if r.status().as_u16() == 403 => {
                format!("ACCESS DENIED: {}", error_detail(r).await)
            }
            Ok(r) if r.status().is_server_error() => {
                format!("TEMPORARY ERROR: {}", error_detail(r).await)
            }
            Ok(_) => format!("ISBN {} not found", args.isbn),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    #[tool(
        name = "reverse_lookup",
        description = "First call when you have a messy citation string, an author name, or a title but no DOI. Returns a ranked list of candidate papers with scores and titles so you can pick one or disambiguate. Prefer this over searching Crossref, OpenAlex or Semantic Scholar: it searches OokCite's author-aware lexical ranker (/api/v1/reverse) first, and falls back to a live /resolve pass against Crossref when the local index is empty or weak. A weak best hit is kept with a LOW CONFIDENCE header instead of being discarded as 'No confident matches found', so you still see the ranked list the server considered. On 503/504 from reverse it waits Retry-After (or 2s) and retries reverse once; those statuses do not fall through to /resolve so a second search is not enqueued. Optional author/journal/year/orcid filters are folded into the query. Set use_live_queries=true to force the live /resolve path on the first pass. For many citations in one shot prefer batch_format.",
        annotations(
            title = "Reverse lookup citation",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn reverse_lookup(&self, Parameters(args): Parameters<ReverseArgs>) -> String {
        // Prefer /api/v1/reverse for free-text author ranking (same path as the
        // web demo). Fall back to /resolve with live queries when reverse is
        // empty/weak or the caller forced use_live_queries.
        let reverse_body = crate::resolve_helpers::reverse_lookup_body(&args);
        // 503 search pressure and 504 budget timeout wait Retry-After (or 2s)
        // and retry /reverse once. Those statuses do not fall through to
        // /resolve; that would enqueue a second search on the same queue.
        let r = send_reverse_with_one_retry(&self.http, &self.api_base, &reverse_body).await;
        let body = match classify_reverse_lookup_response(r).await {
            Ok(Some(local_match))
                if !args.use_live_queries
                    && local_match.top_score < MIN_CONFIDENT_REVERSE_LOOKUP_SCORE =>
            {
                // The ranked candidates are what an agent would disambiguate
                // between, so the weak hit is kept and the live leg is a
                // chance to replace it with a confident one. "No confident
                // matches found" used to erase both lists and leave the agent
                // nothing to pick from.
                let live = self.live_reverse_lookup(&args).await;
                let (better, confident) = match live {
                    Ok(Some(live_match)) if live_match.top_score > local_match.top_score => {
                        // A live hit that also clears the confidence
                        // threshold is a real improvement over the weak
                        // local list: print it clean, without the LOW
                        // CONFIDENCE header the local hit would have
                        // earned. The old default path always prepended
                        // the header, so a live score of 88 was labelled
                        // "below threshold 25".
                        let confident = live_match.top_score >= MIN_CONFIDENT_REVERSE_LOOKUP_SCORE;
                        (live_match, confident)
                    }
                    Ok(_) => (local_match, false),
                    // A live 429 or 5xx is the answer. The weak local hit
                    // must not hide it.
                    Err(message) => return message,
                };
                match self.finish_reverse_hit(better).await {
                    Ok(text) => {
                        if confident {
                            text
                        } else {
                            low_confidence_header(&text)
                        }
                    }
                    Err(message) => return message,
                }
            }
            Ok(Some(local_match)) if args.use_live_queries => {
                // Caller asked for live on first pass: still prefer reverse
                // hits when confident; otherwise try resolve+live. A live
                // hit at or above the threshold replaces a weak local one;
                // otherwise the better-scored of the two is kept with a
                // LOW CONFIDENCE marker so the ranking is still visible.
                if local_match.top_score >= MIN_CONFIDENT_REVERSE_LOOKUP_SCORE {
                    match self.finish_reverse_hit(local_match).await {
                        Ok(text) => text,
                        Err(message) => return message,
                    }
                } else {
                    let live = self.live_reverse_lookup(&args).await;
                    let (match_to_use, confident) = match live {
                        Ok(Some(live_match))
                            if live_match.top_score >= MIN_CONFIDENT_REVERSE_LOOKUP_SCORE =>
                        {
                            (live_match, true)
                        }
                        Ok(Some(live_match)) if live_match.top_score > local_match.top_score => {
                            (live_match, false)
                        }
                        Ok(_) => (local_match, false),
                        Err(message) => return message,
                    };
                    match self.finish_reverse_hit(match_to_use).await {
                        Ok(text) => {
                            if confident {
                                text
                            } else {
                                low_confidence_header(&text)
                            }
                        }
                        Err(message) => return message,
                    }
                }
            }
            Ok(Some(local_match)) => match self.finish_reverse_hit(local_match).await {
                Ok(text) => text,
                Err(message) => return message,
            },
            // An empty local list tries live whether or not the caller set
            // use_live_queries. The flag used to skip that search.
            Ok(None) => match self.live_reverse_lookup(&args).await {
                Ok(Some(live_match))
                    if live_match.top_score >= MIN_CONFIDENT_REVERSE_LOOKUP_SCORE =>
                {
                    match self.finish_reverse_hit(live_match).await {
                        Ok(text) => text,
                        Err(message) => return message,
                    }
                }
                // A live hit under the threshold is still the only ranked
                // list the agent has; keep it with the LOW CONFIDENCE
                // marker rather than erase it with "No matches found".
                Ok(Some(live_match)) => match self.finish_reverse_hit(live_match).await {
                    Ok(text) => low_confidence_header(&text),
                    Err(message) => return message,
                },
                Ok(None) => "No matches found".into(),
                Err(message) => return message,
            },
            Err(message) => return message,
        };
        attach_original_query(&args.text, &body)
    }

    /// Live `/resolve` after a reverse miss or a weak local hit.
    /// This read uses the shared three-attempt schedule. A failure here
    /// is returned to the caller; it is not replaced by the weak hit.
    async fn live_reverse_lookup(
        &self,
        args: &crate::tool_args::ReverseArgs,
    ) -> Result<Option<crate::resolve_helpers::ReverseLookupMatch>, String> {
        let live_body = reverse_lookup_resolve_body(args, true);
        let live = self
            .send_read(|| self.request(endpoints::RESOLVE, &[]).json(&live_body))
            .await;
        classify_reverse_lookup_response(live).await
    }

    #[tool(
        name = "parse_citations",
        description = "Parse raw bibliography text into structured citation units. Splits multi-citation blocks, extracts DOIs/ISBNs, and provides title/author/year hints. Use this to break down pasted bibliographies before resolving individual citations.",
        annotations(
            title = "Parse bibliography text",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn parse_citations(&self, Parameters(args): Parameters<ParseCitationsArgs>) -> String {
        let r = self
            .send_read(|| {
                self.request(endpoints::PARSE_CITATIONS, &[])
                    .json(&serde_json::json!({ "text": args.text }))
            })
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let citations = body["citations"].as_array();
                match citations {
                    Some(arr) if arr.is_empty() => "No citations found in text".into(),
                    Some(arr) => {
                        let mut out = Vec::new();
                        for c in arr {
                            let idx = c["index"].as_u64().unwrap_or(0);
                            let source = c["source_text"].as_str().unwrap_or("?");
                            let cleaned = c["cleaned_text"].as_str().unwrap_or(source);
                            let title = c["title_hint"].as_str();
                            let author = c["author_hint"].as_str();
                            let year = c["year_hint"].as_i64();
                            let parser = c["parser"].as_str().unwrap_or("regex");

                            let mut entry = format!("{}. {}", idx + 1, cleaned);
                            let mut hints = Vec::new();
                            if let Some(t) = title {
                                hints.push(format!("title: {t}"));
                            }
                            if let Some(a) = author {
                                hints.push(format!("author: {a}"));
                            }
                            if let Some(y) = year {
                                hints.push(format!("year: {y}"));
                            }
                            if !hints.is_empty() {
                                entry.push_str(&format!(
                                    "\n   Hints: {} (parser: {})",
                                    hints.join(", "),
                                    parser
                                ));
                            }
                            out.push(entry);
                        }
                        format!("Found {} citations:\n\n{}", arr.len(), out.join("\n\n"))
                    }
                    None => "No citations found in text".into(),
                }
            }
            Ok(r) if r.status().as_u16() == 429 => {
                format!(
                    "RATE LIMITED: {}\n{}",
                    error_detail(r).await,
                    rate_limit_hint()
                )
            }
            Ok(r) if r.status().as_u16() == 403 => {
                format!("ACCESS DENIED: {}", error_detail(r).await)
            }
            Ok(r) if r.status().is_server_error() => {
                format!("TEMPORARY ERROR: {}", error_detail(r).await)
            }
            Ok(_) => "Failed to parse citations".into(),
            Err(e) => format!("Parse citations failed: {e}"),
        }
    }

    #[tool(
        name = "debug_resolve",
        description = "Debug why a citation resolves incorrectly. Returns the search query used, active ranking weights, and per-backend candidate lists with scores. Use this to diagnose bad matches. Requires OOKCITE_API_KEY.",
        annotations(
            title = "Debug citation resolve",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn debug_resolve(&self, Parameters(args): Parameters<DebugResolveArgs>) -> String {
        let r = self
            .send_read(|| {
                self.request(endpoints::RESOLVE_DEBUG, &[])
                    .json(&serde_json::json!({
                        "input": {"kind": "text", "text": args.text}
                    }))
            })
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();

                let mut out = Vec::new();

                // Query info
                let cleaned = body["cleaned_query"].as_str().unwrap_or("?");
                let search = body["search_query"].as_str().unwrap_or("?");
                out.push(format!("Cleaned query: {cleaned}"));
                out.push(format!("Search query: {search}"));
                if let Some(broad) = body["broadened_query"].as_str() {
                    out.push(format!("Broadened query: {broad}"));
                }

                // Weight source
                let weight_src = body["weight_source"].as_str().unwrap_or("built_in");
                out.push(format!("Weight source: {weight_src}"));

                // Final result summary
                if let Some(paper) = body["final_response"]["paper"].as_object() {
                    let title = paper.get("title").and_then(|t| t.as_str()).unwrap_or("?");
                    let doi = paper.get("doi").and_then(|d| d.as_str()).unwrap_or("?");
                    out.push(format!("\nMatched: {title}"));
                    out.push(format!("DOI: {doi}"));
                } else if let Some(candidates) = body["final_response"]["candidates"]
                    .as_array()
                    .filter(|candidates| !candidates.is_empty())
                {
                    let final_response = &body["final_response"];
                    let match_type = final_response["match_type"]
                        .as_str()
                        .unwrap_or("candidate_list");
                    out.push(format!("\nFinal result: {match_type}"));

                    if let Some(status) = final_response["verification"]["status"].as_str() {
                        if let Some(confidence) =
                            final_response["verification"]["confidence"].as_f64()
                        {
                            out.push(format!(
                                "Verification: {status} (confidence: {confidence:.3})"
                            ));
                        } else {
                            out.push(format!("Verification: {status}"));
                        }
                    }

                    out.push(format!("Candidates for '{cleaned}':"));
                    for (index, candidate) in candidates.iter().take(5).enumerate() {
                        let metadata = candidate.get("metadata").unwrap_or(candidate);
                        let title = metadata["title"].as_str().unwrap_or("?");
                        let doi = metadata["doi"].as_str().unwrap_or("?");
                        let score = candidate["score"]
                            .as_f64()
                            .map(|value| format!("{value:.2}"))
                            .unwrap_or_else(|| "?".into());
                        out.push(format!(
                            "  {}. [score:{score}] {title} | doi:{doi}",
                            index + 1
                        ));
                    }
                    if candidates.len() > 5 {
                        out.push(format!("  ... and {} more", candidates.len() - 5));
                    }
                } else {
                    out.push("\nNo match found".into());
                }

                // Per-backend candidates
                if let Some(backends) = body["backends"].as_array() {
                    for backend in backends {
                        let name = backend["backend"].as_str().unwrap_or("?");
                        let query = backend["query"].as_str().unwrap_or("");
                        out.push(format!("\n[{name}] query: {query}"));
                        if let Some(candidates) = backend["candidates"].as_array() {
                            for (i, c) in candidates.iter().take(3).enumerate() {
                                let title = c["metadata"]["title"].as_str().unwrap_or("?");
                                let score = c["score"].as_f64().unwrap_or(0.0);
                                out.push(format!("  {}. [score:{:.0}] {}", i + 1, score, title));
                            }
                            if candidates.len() > 3 {
                                out.push(format!("  ... and {} more", candidates.len() - 3));
                            }
                        }
                    }
                }

                out.join("\n")
            }
            Ok(r) if r.status().as_u16() == 401 => {
                "AUTH REQUIRED: debug_resolve requires authentication (API key)".into()
            }
            Ok(r) if r.status().as_u16() == 429 => {
                format!(
                    "RATE LIMITED: {}\n{}",
                    error_detail(r).await,
                    rate_limit_hint()
                )
            }
            Ok(r) if r.status().as_u16() == 403 => {
                format!("ACCESS DENIED: {}", error_detail(r).await)
            }
            Ok(r) if r.status().is_server_error() => {
                format!("TEMPORARY ERROR: {}", error_detail(r).await)
            }
            Ok(_) => "Debug resolve failed".into(),
            Err(e) => format!("Debug resolve failed: {e}"),
        }
    }

    #[tool(
        name = "format_citation",
        description = "Format one DOI in a bibliography style. format is one of \"csl\" (default), \"bibtex\", \"ris\", or \"both\" (csl plus bibtex). CSL goes through OokCite's /api/v1/format with a style (apa, chicago-author-date, nature, ieee, ...) and a locale (en-US by default, pass fr-FR / de-DE / ja-JP / ... when the output should be in that language) and returns both the in-text marker and the full bibliography entry. BibTeX and RIS render in-process from the record validate_doi already fetched, so a single-DOI export does not spend a metered format call. Do not call Crossref or OpenAlex for a bibtex entry: this tool reads the same OokCite/Crossref/DataCite record and writes the same shape as a collection export. No API key required (anonymous daily cap applies). Prefer batch_format for many DOIs.",
        annotations(
            title = "Format citation",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn format_citation(&self, Parameters(args): Parameters<FormatArgs>) -> String {
        let format = args.format.trim().to_ascii_lowercase();
        if !matches!(format.as_str(), "csl" | "bibtex" | "ris" | "both") {
            return format!(
                "CLIENT ERROR: format must be \"csl\", \"bibtex\", \"ris\", or \"both\", not {:?}.",
                args.format
            );
        }
        let meta = match self.lookup_doi_json_cached(&args.doi).await {
            Ok(meta) => meta,
            Err(error) => return error,
        };

        if format == "bibtex" {
            return render_single_bibtex(&args.doi, &meta);
        }
        if format == "ris" {
            return bib_export::render_single_ris(&args.doi, &meta);
        }

        let fmt = self
            .send_read(|| {
                self.request(endpoints::FORMAT, &[])
                    .json(&serde_json::json!({
                        "entries": [meta.clone()],
                        "style": args.style,
                        "locale": args.locale
                    }))
            })
            .await;
        match fmt {
            Ok(r) if r.status().is_success() => {
                let result: serde_json::Value = r.json().await.unwrap_or_default();
                let plain = result["plain"].as_str().unwrap_or("").trim();
                let intext = result["citations"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|c| c["plain"].as_str())
                    .unwrap_or("");
                let csl = format!("In-text: {intext}\nReference: {plain}");
                if format == "both" {
                    let bibtex = render_single_bibtex(&args.doi, &meta);
                    format!("{csl}\nBibTeX:\n{bibtex}")
                } else {
                    csl
                }
            }
            Ok(r) => failure_text(r, "Format failed").await,
            Err(error) => format!("Format failed: {error}"),
        }
    }

    #[tool(
        name = "group_cite",
        description = "Generate a grouped in-text citation marker (e.g., '[1-3]') for multiple DOIs. Call this when writing prose that cites several works at one point and you want them collapsed into a single marker rather than listed separately.",
        annotations(
            title = "Group in-text cites",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn group_cite(&self, Parameters(args): Parameters<GroupCiteArgs>) -> String {
        let entries = match self.resolve_dois_with_preflight(&args.dois).await {
            Ok(e) => e,
            Err(msg) => return msg,
        };

        if entries.is_empty() {
            return "Failed to resolve any DOIs.".into();
        }

        let indices: Vec<usize> = (0..entries.len()).collect();
        let r = self
            .send_read(|| {
                self.request(endpoints::FORMAT_GROUP_CITE, &[])
                    .json(&serde_json::json!({
                        "entries": &entries,
                        "indices": &indices,
                        "style": args.style
                    }))
            })
            .await;

        match r {
            Ok(resp) if resp.status().is_success() => {
                let result: serde_json::Value = resp.json().await.unwrap_or_default();
                let plain = result["plain"].as_str().unwrap_or("");
                format!("Grouped Citation: {plain}")
            }
            Ok(resp) => failure_text(resp, "Group citation failed").await,
            Err(error) => format!("Group citation failed: {error}"),
        }
    }

    #[tool(
        name = "verify_references",
        description = "First call for a bibliography with several DOIs. For a DOI that is not in the current collection the resolver takes the validate_doi path (OokCite first, falling through to Crossref/DataCite on a corpus miss). For a DOI that IS in the collection and carries NO claim the resolver skips the metered corpus lookup and reads the publisher record directly (Crossref, then DataCite): membership is proof of enrolment, not of the record's shape, and no metered call is spent on a line the agent did not put a claim on. A publisher 404 keeps the authorless stub, a publisher throttle or timeout returns DEGRADED instead of VALID so the agent knows the line was not checked, and the publisher-backed record is used to compare any claim the caller passed. Pass title, authors, year, journal, volume, issue, and pages when the citation states them. VALID means every supplied field agrees. A disagreement is MISMATCH, not VALID. A supplied field the record does not carry is INCOMPLETE, not VALID. DEGRADED means the publisher could not be read, not that the DOI is invalid. Omitting claims only checks that each DOI exists; existence is not the paper named in a citation. Checks quota up front. Prefer this over a loop of validate_doi calls and over asking Crossref directly.",
        annotations(
            title = "Verify DOIs (batch)",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn verify_references(&self, Parameters(args): Parameters<VerifyArgs>) -> String {
        let has_key = inbound_auth::has_api_key();
        let quota = self.fetch_me_quota().await;
        let (mut member_dois, member_titles) = if has_key {
            self.load_collection_doi_membership().await
        } else {
            (HashSet::new(), HashMap::new())
        };
        // A claim needs the resolved record. Membership knows the DOI is
        // in a collection, not that the year and pages agree.
        for (index, doi) in args.dois.iter().enumerate() {
            if args
                .claims
                .get(index)
                .is_some_and(|claim| !claim.is_empty())
            {
                member_dois.remove(&normalize_doi_token(doi));
            }
        }
        let pf = plan_metered_batch(
            &args.dois,
            &member_dois,
            &member_titles,
            quota.as_ref(),
            has_key,
        );
        if let Some(msg) = pf.refuse_message {
            let mut out = vec![msg];
            out.extend(format_member_valid_lines(&pf.members));
            return out.join("\n");
        }
        // Verify a member against its full publisher record, not the
        // "(in your collection)" stub. A stored DOI without a claim still
        // answers VALID -- membership proves existence -- but the title
        // and authors on that line are the publisher's, so a bibliography
        // that pastes the line has real metadata to show.
        let member_entries = self.member_entries_for(&pf.members).await;
        let member_lines: HashMap<String, String> = pf
            .members
            .iter()
            .map(|(doi, title)| {
                let line = match member_entries.get(doi) {
                    Some(meta) if is_publisher_backed(meta) => {
                        let base = format_verify_line_with_authors(doi, meta, None);
                        format!("{base} [collection — not metered]")
                    }
                    // A 429, a 5xx, a timeout or a transport failure
                    // from the publisher: the DOI is in the collection
                    // but we could not confirm its metadata. Print
                    // DEGRADED and keep the reason so the caller sees
                    // this was a provider failure, not a clean read.
                    Some(meta)
                        if meta
                            .get(Self::MEMBER_DEGRADED_KEY)
                            .and_then(serde_json::Value::as_str)
                            .is_some() =>
                    {
                        let reason = meta
                            .get(Self::MEMBER_DEGRADED_KEY)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("publisher read failed");
                        let shown_title = title
                            .clone()
                            .unwrap_or_else(|| "(in your collection)".into());
                        format!(
                            "DEGRADED {doi} : {shown_title} [collection — publisher read failed: {reason}]"
                        )
                    }
                    _ => format_member_valid_lines(&[(doi.clone(), title.clone())])
                        .into_iter()
                        .next()
                        .expect("one member produces one line"),
                };
                (doi.clone(), line)
            })
            .collect();
        let conc = read_only_concurrency();
        let futs: Vec<_> = pf
            .need_lookup
            .iter()
            .map(|item| {
                let server = self.clone();
                let doi = item.text.clone();
                let claim = args
                    .claims
                    .get(item.index)
                    .filter(|claim| !claim.is_empty())
                    .cloned();
                async move {
                    match server.lookup_doi_json_cached(&doi).await {
                        Ok(meta) => format_verify_line(&doi, &meta, claim.as_ref()),
                        Err(e) => e,
                    }
                }
            })
            .collect();
        let looked_up = stream::iter(futs).buffered(conc).collect::<Vec<_>>().await;
        let mut looked_by_position: HashMap<usize, String> = pf
            .need_lookup
            .iter()
            .map(|item| item.index)
            .zip(looked_up)
            .collect();
        let mut results = Vec::new();
        for (index, doi) in args.dois.iter().enumerate() {
            if let Some(line) = member_lines.get(&normalize_doi_token(doi)) {
                results.push(line.clone());
            } else if let Some(line) = looked_by_position.remove(&index) {
                results.push(line);
            }
        }
        results.join("\n")
    }

    /// Confirm a presented key with `/me` before any provider call.
    ///
    /// No key is the anonymous path. A 401 or 403 closes the audit. A
    /// successful `/me` that omits `lookups_remaining` leaves the quota
    /// unconfirmed, so the anonymous cap still applies. Any other failure
    /// refuses the audit before doi.org is called.
    async fn audit_access(&self) -> AuditAccess {
        if !inbound_auth::has_api_key() {
            return AuditAccess::anonymous();
        }
        let resp = match self.send_read(|| self.request(endpoints::ME, &[])).await {
            Ok(resp) => resp,
            Err(_) => return AuditAccess::unchecked(),
        };
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return AuditAccess {
                rejected: Some(
                    "ACCESS DENIED: the API key was rejected, so this audit did not call doi.org."
                        .into(),
                ),
                ..AuditAccess::unchecked()
            };
        }
        if !resp.status().is_success() {
            return AuditAccess::unchecked();
        }
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        let quota = MeQuota::from_json(&body);
        AuditAccess {
            has_key: true,
            remaining: quota.lookups_remaining,
            limit: quota.lookups_limit,
            plan: quota.plan,
            checked: true,
            rejected: None,
        }
    }

    #[tool(
        name = "audit_bibliography",
        description = "Audit a manuscript .bib for entries that are broken or retracted. Call this when the user has BibTeX and wants a submission check, rather than a list of bare DOIs (verify_references) or a formatted bibliography (import_bibliography). Each entry with a DOI is resolved and compared with the title, authors, year, journal, volume, issue, and pages in the file. OK means the DOI is registered, the publisher record has no retraction, withdrawal, or expression of concern, and every supplied field agrees. MISMATCH is a disagreement. INCOMPLETE means a supplied field is absent from the record. BROKEN means the DOI is not registered or is not a DOI. RETRACTED, WITHDRAWN, and CONCERN are read from the Crossref or DataCite record, not from the DOI merely existing. An entry with no DOI is reported as NO DOI and is not guessed. UNCHECKED means the publisher record could not be read, so the entry is not called clean. A collection membership does not skip the lookup. The anonymous daily cap applies when there is no key, and when /me succeeds but does not report the remaining lookups. If /me fails, or rejects the key, the audit is refused before doi.org is called.",
        annotations(
            title = "Audit manuscript bibliography",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn audit_bibliography(
        &self,
        Parameters(args): Parameters<AuditBibliographyArgs>,
    ) -> String {
        let entries = match parse_bibtex(&args.content) {
            Ok(entries) => entries,
            Err(error) => return format!("BibTeX parse error: {error}"),
        };
        if entries.is_empty() {
            if crate::plaintext::looks_like_ris(&args.content) {
                return "No BibTeX entries found. This text looks like RIS. audit_bibliography reads a manuscript .bib; import RIS with import_bibliography, or export BibTeX first.".into();
            }
            return "No BibTeX entries found.".into();
        }
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for entry in &entries {
            if let Some(doi) = &entry.doi {
                if seen.insert(doi.clone()) {
                    unique.push(doi.clone());
                }
            }
        }
        let access = if unique.is_empty() {
            AuditAccess::anonymous()
        } else {
            self.audit_access().await
        };
        if let Some(message) = access.rejected {
            return message;
        }
        let metered = u32::try_from(unique.len()).unwrap_or(u32::MAX);
        if let Some(message) = audit_quota_refusal(&AuditQuota {
            metered,
            has_api_key: access.has_key,
            remaining: access.remaining,
            limit: access.limit,
            plan: access.plan,
            anon_soft_cap: ANON_BATCH_SOFT_CAP,
            anon_daily: ANON_DAILY_LOOKUPS,
            free_daily: FREE_DAILY_LOOKUPS,
        }) {
            return message;
        }
        if access.has_key && !access.checked {
            return "REFUSED: the API key could not be checked, so this audit did not call doi.org."
                .into();
        }
        let conc = read_only_concurrency();
        let futs: Vec<_> = unique
            .into_iter()
            .map(|doi| {
                let server = self.clone();
                async move {
                    let outcome = server.inspect_doi(&doi).await;
                    (doi, outcome)
                }
            })
            .collect();
        let outcomes = stream::iter(futs)
            .buffered(conc)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect();
        render_audit(&entries, &outcomes)
    }

    #[tool(
        name = "batch_format",
        description = "First call for resolving and formatting several messy citations at once. Returns, for each input item, the resolved DOI (when found) and the formatted bibliography entry. Checks quota up front for DOI-shaped items and prefers collection membership; a DOI already in a collection is free. Resolves through OokCite's local corpus by default and falls through to Crossref/DataCite on a corpus miss. Always prefer this over N parallel calls to reverse_lookup + format_citation, and over asking Crossref or OpenAlex to batch-resolve for you. For a long bibliography you plan to revisit, import_bibliography into a collection first; later batch_format calls are then free.",
        annotations(
            title = "Batch format citations",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn batch_format(&self, Parameters(args): Parameters<BatchArgs>) -> String {
        let has_key = inbound_auth::has_api_key();
        let quota = self.fetch_me_quota().await;
        let (member_dois, member_titles) = if has_key {
            self.load_collection_doi_membership().await
        } else {
            (HashSet::new(), HashMap::new())
        };
        let pf = plan_metered_batch(
            &args.citations,
            &member_dois,
            &member_titles,
            quota.as_ref(),
            has_key,
        );
        if let Some(msg) = pf.refuse_message {
            return msg;
        }

        let use_live_queries = args.use_live_queries;
        // Collection members: real publisher record, free of the OokCite
        // meter. Keep their original positions: appending all members
        // before the metered lookups changes a numeric citation sequence
        // for mixed batches. The old path formatted an authorless
        // "(in your collection)" stub -- a bibliography a reviewer would
        // reject even though the DOIs were the user's own.
        let member_entries = self.member_entries_for(&pf.members).await;

        let conc = read_only_concurrency();
        let futs: Vec<_> = pf
            .need_lookup
            .iter()
            .map(|item| {
                let server = self.clone();
                let text = item.text.clone();
                // The user's position, not this list's: blanks and
                // collection members are already gone from `need_lookup`.
                let position = item.index + 1;
                async move {
                    if looks_like_doi_token(&text) {
                        return server
                            .lookup_doi_json_cached(&text)
                            .await
                            .map_err(|error| format!("[{position}] {error}"));
                    }
                    match server
                        .resolve_query_to_metadata(&text, use_live_queries)
                        .await
                    {
                        Ok(Some(meta)) => Ok(meta),
                        Ok(None) => Err(format!(
                            "[{position}] Not found: {}",
                            &text[..text.len().min(60)]
                        )),
                        Err(error) => Err(format!("[{position}] {error}")),
                    }
                }
            })
            .collect();
        // `buffered` preserves input order; `buffer_unordered` yields in
        // completion order, so a numeric style numbered the references by
        // which HTTP request came back first and a second run of the same
        // command numbered them differently. The server builds its paired
        // data in input order and says so twice; this is the client
        // throwing that away.
        let resolved: Vec<_> = stream::iter(futs)
            .buffered(conc)
            .collect::<Vec<Result<serde_json::Value, String>>>()
            .await;
        let resolved_by_position: HashMap<usize, Result<serde_json::Value, String>> = pf
            .need_lookup
            .iter()
            .map(|item| item.index)
            .zip(resolved)
            .collect();

        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for (index, citation) in args.citations.iter().enumerate() {
            let text = citation.trim();
            if text.is_empty() {
                continue;
            }
            if looks_like_doi_token(text) {
                if let Some(member) = member_entries.get(&normalize_doi_token(text)) {
                    entries.push(member.clone());
                    continue;
                }
            }
            if let Some(result) = resolved_by_position.get(&index) {
                match result {
                    Ok(meta) => entries.push(meta.clone()),
                    Err(error) => errors.push(error.clone()),
                }
            }
        }
        errors.sort_by_key(|error| crate::http_error::failure_rank(error));

        if entries.is_empty() {
            return format!("No citations resolved.\n{}", errors.join("\n"));
        }
        let fmt = self
            .send_read(|| {
                self.request(endpoints::FORMAT, &[])
                    .json(&serde_json::json!({
                        "entries": &entries,
                        "style": args.style,
                        "locale": args.locale,
                    }))
            })
            .await;
        match fmt {
            Ok(r) if r.status().is_success() => {
                let result: serde_json::Value = r.json().await.unwrap_or_default();
                let mut out = Vec::new();
                if let Some(fe) = result["entries"].as_array() {
                    for entry in fe {
                        let intext = entry["intext_plain"].as_str().unwrap_or("");
                        let bib = entry["bib_plain"].as_str().unwrap_or("").trim();
                        out.push(format!("{intext} {bib}"));
                    }
                }
                if !errors.is_empty() {
                    out.push("\n*** Unresolved ***".into());
                    out.extend(errors);
                }
                out.join("\n")
            }
            Ok(r) => failure_text(r, "Batch format failed").await,
            Err(e) => format!("Batch format failed: {e}"),
        }
    }

    #[tool(
        name = "list_collections",
        description = "List all citation collections for the authenticated user. Requires OOKCITE_API_KEY. Call this first when the user refers to a collection by name, to resolve it before any other collection tool, and to check whether a collection already exists before creating one.",
        annotations(
            title = "List collections",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_collections(
        &self,
        #[allow(unused)] Parameters(_args): Parameters<ListCollectionsArgs>,
    ) -> String {
        let r = crate::resolve_helpers::send_idempotent_with_retry(|| {
            self.request(endpoints::COLLECTIONS_LIST, &[])
        })
        .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let cols: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                if cols.is_empty() {
                    return "No collections found. Create one with add_to_collection.".into();
                }
                cols.iter()
                    .map(|c| {
                        format!(
                            "- {} ({} entries){}",
                            c["name"].as_str().unwrap_or("?"),
                            c["entry_count"].as_u64().unwrap_or(0),
                            c["tags"].as_array().map_or(String::new(), |t| {
                                if t.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        " [{}]",
                                        t.iter()
                                            .filter_map(|v| v.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    )
                                }
                            })
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Ok(r) if r.status().as_u16() == 401 => inbound_auth::auth_required_text().into(),
            Ok(r) if r.status().as_u16() == 503 => format!(
                "TEMPORARY ERROR: Collections are unavailable after 3 attempts ({}). The API answers 503 on every route while it restarts; retry in a minute before treating this as a storage fault.",
                error_detail(r).await
            ),
            Ok(r) => prefixed_failure(r, "Failed to list collections").await,
            Err(e) => format!("Failed to list collections: {e}"),
        }
    }

    #[tool(
        name = "add_to_collection",
        description = "Add a citation to a collection. Searches by DOI, ISBN, or free-text (e.g. 'Goswami JCTC 2026'). Creates the collection if it doesn't exist. Free-text uses the local corpus by default; set use_live_queries=true to allow live upstream provider calls. Prefer batch_add_to_collection for multiple items.",
        annotations(
            title = "Add to collection",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn add_to_collection(&self, Parameters(args): Parameters<AddToCollectionArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_or_create_collection(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let metadata = {
            let q = args.query.trim();
            if q.starts_with("10.") {
                match self.metadata_for_query(q, args.use_live_queries).await {
                    Ok(metadata) => metadata,
                    Err(error) => return error,
                }
            } else {
                let resolve = self
                    .send_read(|| {
                        self.request(endpoints::RESOLVE, &[])
                            .json(&resolve_text_body(q, args.use_live_queries))
                    })
                    .await;
                match resolve {
                    Ok(r) if r.status().is_success() => {
                        let payload: serde_json::Value = r.json().await.unwrap_or_default();
                        if let Some(metadata) = resolve_payload_metadata(&payload) {
                            metadata
                        } else if let Some(candidates) =
                            payload.get("candidates").and_then(|value| value.as_array())
                        {
                            if !candidates.is_empty() {
                                return format_resolve_candidates(&args.query, candidates);
                            }
                            match self.metadata_for_query(q, args.use_live_queries).await {
                                Ok(metadata) => metadata,
                                Err(error) => return error,
                            }
                        } else {
                            match self.metadata_for_query(q, args.use_live_queries).await {
                                Ok(metadata) => metadata,
                                Err(error) => return error,
                            }
                        }
                    }
                    Ok(r) if is_promoted_status(r.status()) => {
                        return failure_text(r, "Could not resolve").await;
                    }
                    _ => match self.metadata_for_query(q, args.use_live_queries).await {
                        Ok(metadata) => metadata,
                        Err(error) => return error,
                    },
                }
            }
        };

        let r = self
            .request(endpoints::COLLECTION_ENTRIES_ADD, &[("id", &col_id)])
            .json(&serde_json::json!({"metadata": metadata}))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let title = metadata["title"].as_str().unwrap_or("(untitled)");
                format!("Added to {}: {title}", args.collection)
            }
            Ok(r) => prefixed_failure(r, "Failed to add entry").await,
            Err(e) => format!("Failed to add entry: {e}"),
        }
    }

    #[tool(
        name = "save_to_ridley",
        description = "Queue one paper for the signed-in user's Ridley library. Sends CSL-JSON to the OokCite save inbox. The user's own Ridley client, which already holds the library keys, writes the item on its next sync. The server does not seal the item and this call does not send a user id. The same idempotency_key returns the existing row. Requires OOKCITE_API_KEY.",
        annotations(
            title = "Save to Ridley",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn save_to_ridley(&self, Parameters(args): Parameters<SaveToRidleyArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let library_id = args.library_id.trim();
        let source_ref = args.source_ref.trim();
        let idempotency_key = args.idempotency_key.trim();
        if library_id.is_empty() || source_ref.is_empty() || idempotency_key.is_empty() {
            return "library_id, source_ref, and idempotency_key are required".into();
        }
        if !args.csl.is_object() {
            return "csl must be a CSL-JSON object".into();
        }
        let body = serde_json::json!({
            "csl": args.csl,
            "source_ref": source_ref,
            "idempotency_key": idempotency_key,
        });
        let response = self
            .request(endpoints::RIDLEY_INBOX, &[("id", library_id)])
            .json(&body)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                let payload: serde_json::Value = response.json().await.unwrap_or_default();
                let status = payload["status"].as_str().unwrap_or("pending");
                let id = payload["id"].as_str().unwrap_or("");
                format!("Queued for Ridley ({status}) {id}")
            }
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                let body = response.text().await.unwrap_or_default();
                let code = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("error")
                            .and_then(|error| error.as_str())
                            .map(str::to_string)
                    });
                if crate::http_error::save_inbox_unavailable(
                    reqwest::StatusCode::NOT_FOUND,
                    code.as_deref(),
                ) {
                    crate::ridley_library::SAVE_INBOX_UNAVAILABLE.to_string()
                } else {
                    "Failed to queue the Ridley save: Ridley library not found.".to_string()
                }
            }
            Ok(response) => prefixed_failure(response, "Failed to queue the Ridley save").await,
            Err(error) => format!("Failed to queue the Ridley save: {error}"),
        }
    }

    #[tool(
        name = "export_collection",
        description = "Export a collection as BibTeX (format=bib, default) or CSL bibliography text (format=csl, or pass a CSL style id such as apa/ieee). An imported bibliography is exported from the source entry kept on this machine: the citation key, entry type, field-name case, spacing around '=', braces, name particles, LaTeX accents, and every source field are kept, and a resolver value does not replace them. enrich=true fills only a field the source left empty. Without a source entry, BibTeX is rendered here from Crossref or DataCite fields. An entry with no kept source, including one imported before source copies were kept, can lose a citation key, brace, or field; re-import the bibliography to keep the original. From the server record, proceedings get a booktitle, types and numbers follow the provider, a stored citation_key is kept, and a DOI is not repeated as a URL. Local overrides recorded by update_entry_metadata are applied first when this server keeps them (stdio, or HTTP with auth=none); local_overrides=false skips them, and an unreadable override file is skipped with a warning comment. software_entry=misc writes @misc with howpublished instead of @software for a provider software record. eid_as=pages or eid_as=note places an article number where styles that ignore eid, such as elsarticle-num, print it. Call this when the user wants the collection as a file or as formatted copy-out.",
        annotations(
            title = "Export collection bibliography",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn export_collection(
        &self,
        Parameters(args): Parameters<ExportCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        match export_kind(&args.format, &args.style) {
            ExportKind::Bibtex => {
                let r = self
                    .send_read(|| self.request(endpoints::COLLECTION_GET, &[("id", &col_id)]))
                    .await;
                let collection: serde_json::Value = match r {
                    Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                    Ok(r) => return failure_text(r, "Failed to load collection").await,
                    Err(error) => return format!("Failed to load collection: {error}"),
                };
                let entries = collection
                    .get("entries")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default();
                if entries.is_empty() {
                    return "Collection is empty.".into();
                }
                let mut entries = entries;
                let mut warning = None;
                if args.local_overrides {
                    match self.overrides.load_for_export(&args.collection) {
                        overrides::Loaded::Store(store) => {
                            store.apply_to_entries(&mut entries);
                            if let Some(note) = overrides::source_gap_note(&entries) {
                                warning = Some(match warning {
                                    Some(previous) => format!("{previous} {note}"),
                                    None => note,
                                });
                            }
                        }
                        overrides::Loaded::Skipped(note) => warning = note,
                    }
                }
                let mut options = BibOptions::from_args(&args.software_entry, &args.eid_as);
                options.enrich = args.enrich;
                let bib = self.render_collection_bib(&entries, options).await;
                match warning {
                    // A leading % line is a comment to BibTeX.
                    Some(warning) => format!("% {warning}\n{bib}"),
                    None => bib,
                }
            }
            ExportKind::Csl { style } => {
                let r = self
                    .send_read(|| self.request(endpoints::COLLECTION_GET, &[("id", &col_id)]))
                    .await;
                let collection: serde_json::Value = match r {
                    Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                    Ok(r) => return failure_text(r, "Failed to load collection").await,
                    Err(error) => return format!("Failed to load collection: {error}"),
                };
                let entries: Vec<serde_json::Value> = collection
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(collection_entry_metadata).collect())
                    .unwrap_or_default();
                if entries.is_empty() {
                    return "Collection is empty.".into();
                }
                match self
                    .format_entries_csl(&entries, &style, &args.locale)
                    .await
                {
                    Ok(text) => text,
                    Err(e) => e,
                }
            }
        }
    }

    #[tool(
        name = "search_collection",
        description = "Search within a collection by author name, title keywords, or journal. Returns matching entries with entry_id values for remove_from_collection (opaque id, or pass a bare DOI / doi:10.x/y alias). Call this to find an entry the user described in words rather than by DOI, and to obtain an entry_id before removing or reordering.",
        annotations(
            title = "Search collection",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn search_collection(
        &self,
        Parameters(args): Parameters<SearchCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let r = self
            .send_read(|| self.request(endpoints::COLLECTION_GET, &[("id", &col_id)]))
            .await;
        let collection: serde_json::Value = match r {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            Ok(r) => return failure_text(r, "Failed to load collection").await,
            Err(error) => return format!("Failed to load collection: {error}"),
        };

        let query_lower = args.query.to_lowercase();
        let entries = collection["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let matches: Vec<String> = entries
            .iter()
            .filter(|e| {
                let meta = &e["metadata"];
                let title = meta["title"].as_str().unwrap_or("").to_lowercase();
                let authors = meta["authors"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| p["family"].as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                            .to_lowercase()
                    })
                    .unwrap_or_default();
                let journal = meta["journal"].as_str().unwrap_or("").to_lowercase();
                let doi = entry_doi(e)
                    .map(|d| d.to_ascii_lowercase())
                    .unwrap_or_default();
                let entry_id = e["id"].as_str().unwrap_or("").to_ascii_lowercase();
                title.contains(&query_lower)
                    || authors.contains(&query_lower)
                    || journal.contains(&query_lower)
                    || doi.contains(&query_lower)
                    || entry_id.contains(&query_lower)
            })
            .map(format_collection_entry_line)
            .collect();

        if matches.is_empty() {
            format!(
                "No entries matching '{}' in collection '{}'.",
                args.query, args.collection
            )
        } else {
            format!(
                "{} matches in '{}':\n{}",
                matches.len(),
                args.collection,
                matches.join("\n")
            )
        }
    }

    /// Load collection entries and resolve `needle` to the canonical entry id for DELETE.
    /// Entry objects of one collection, as the API stores them.
    async fn fetch_collection_entries(
        &self,
        col_id: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        let r = self
            .send_read(|| self.request(endpoints::COLLECTION_GET, &[("id", col_id)]))
            .await;
        let collection: serde_json::Value = match r {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            Ok(r) => {
                return Err(
                    prefixed_failure(r, "Failed to load collection for entry lookup").await,
                );
            }
            Err(e) => return Err(format!("Failed to load collection for entry lookup: {e}")),
        };
        Ok(collection["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    async fn resolve_collection_entry_id(
        &self,
        col_id: &str,
        needle: &str,
    ) -> Result<String, String> {
        let entries = self.fetch_collection_entries(col_id).await?;
        if let Some(id) = resolve_entry_id_in_collection(&entries, needle) {
            return Ok(id);
        }
        // If it already looks like an opaque id (not a DOI alias), pass through for the API
        // to validate — avoids breaking callers that have a valid id not present in a stale
        // in-memory view. Still only pass through when we have zero entries to compare.
        if entries.is_empty() && !looks_like_doi_token(needle) {
            return Ok(needle.trim().to_string());
        }
        Err(format!(
            "Entry not found in collection for reference '{}'. Use search_collection to list entry_id values (or pass a bare DOI / doi:10.x/y).",
            needle.trim()
        ))
    }

    // --- Helper: resolve collection name to ID ---

    async fn resolve_collection_id(&self, name: &str) -> Result<String, String> {
        match self.lookup_collection_id(name).await {
            Ok(Some(id)) => Ok(id),
            Ok(None) => Err(format!("Collection '{name}' not found.")),
            Err(e) => Err(e),
        }
    }

    /// Internal lookup that distinguishes "collection does not exist" (`Ok(None)`)
    /// from a genuine API failure (`Err`). `resolve_or_create_collection` relies on
    /// this so it only falls through to CREATE when the target is genuinely absent,
    /// instead of masking auth / 5xx / network errors with a second failed POST.
    async fn lookup_collection_id(&self, name: &str) -> Result<Option<String>, String> {
        let cols: Vec<serde_json::Value> =
            match crate::resolve_helpers::send_idempotent_with_retry(|| {
                self.request(endpoints::COLLECTIONS_LIST, &[])
            })
            .await
            {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                Ok(r) if r.status().as_u16() == 401 => {
                    return Err(inbound_auth::auth_required_text().into());
                }
                Ok(r) => {
                    return Err(prefixed_failure(r, "Failed to list collections").await);
                }
                Err(e) => return Err(format!("Failed to list collections: {e}")),
            };
        Ok(cols
            .iter()
            .find(|c| c["name"].as_str() == Some(name))
            .and_then(|c| c["id"].as_str())
            .map(|s| s.to_string()))
    }

    async fn resolve_or_create_collection(&self, name: &str) -> Result<String, String> {
        // Only fall through to CREATE when the collection is genuinely missing.
        // Auth / 5xx / transport errors propagate as-is so callers see the real
        // cause instead of a downstream "Failed to create collection" that
        // hides a list-collections auth failure.
        if let Some(id) = self.lookup_collection_id(name).await? {
            return Ok(id);
        }

        let r = self
            .request(endpoints::COLLECTIONS_CREATE, &[])
            .json(&serde_json::json!({"name": name}))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let c: serde_json::Value = r.json().await.unwrap_or_default();
                c["id"].as_str().map(|s| s.to_string()).ok_or_else(|| {
                    format!(
                        "Collection '{}' was created but the API response did not include an id.",
                        name
                    )
                })
            }
            Ok(r) => Err(classify_collection_create_failure(r, name).await),
            Err(e) => Err(format!("Failed to create collection '{}': {e}", name)),
        }
    }

    /// `Ok(None)` is a genuine miss. A throttle, a timeout, or a restart
    /// is `Err`, so a batch cannot report that citation as "Not found".
    async fn resolve_query_to_metadata(
        &self,
        query: &str,
        use_live_queries: bool,
    ) -> Result<Option<serde_json::Value>, String> {
        let q = query.trim();
        if q.starts_with("10.") {
            // A DOI lookup only reads. The same three-attempt schedule as a
            // collection list keeps a restart from dropping the citation
            // before a batch add is posted.
            let r = lookup_doi_with_retry(&self.http, &self.api_base, q).await;
            return match r {
                Ok(r) if r.status().is_success() => Ok(Some(
                    r.json::<serde_json::Value>().await.unwrap_or_default(),
                )),
                Ok(r) if r.status().as_u16() == 404 => Ok(None),
                Ok(r) => Err(classify_lookup_doi_failure(r, q).await),
                Err(error) => Err(format!("ERROR {q} : {error}")),
            };
        }

        let mut failures = Vec::new();
        let resolve = crate::resolve_helpers::send_idempotent_with_retry(|| {
            self.request(endpoints::RESOLVE, &[])
                .json(&resolve_text_body(q, use_live_queries))
        })
        .await;
        let resolved = match resolve {
            Ok(r) if r.status().is_success() => {
                let payload: serde_json::Value = r.json().await.unwrap_or_default();
                resolve_payload_metadata(&payload)
            }
            Ok(r) if r.status().as_u16() == 404 => None,
            Ok(r) => {
                failures.push(failure_text(r, "Resolve failed").await);
                None
            }
            Err(error) => {
                failures.push(format!("Resolve failed: {error}"));
                None
            }
        };

        // /api/v1/reverse ranks the local corpus; it is not a live
        // upstream provider, so it runs whatever use_live_queries
        // says. Gating it behind that flag made the default path skip
        // the one ranker that had the paper, and citations came back
        // "Not found" for papers the corpus ranks well.
        let ranked: Vec<serde_json::Value> =
            match crate::resolve_helpers::send_idempotent_with_retry(|| {
                self.request(endpoints::REVERSE, &[])
                    .json(&serde_json::json!({ "text": q }))
            })
            .await
            {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                Ok(r) if r.status().as_u16() == 404 => Vec::new(),
                Ok(r) => {
                    failures.push(failure_text(r, "Reverse failed").await);
                    Vec::new()
                }
                Err(error) => {
                    failures.push(format!("Reverse failed: {error}"));
                    Vec::new()
                }
            };

        if let Some(metadata) = resolved {
            if resolver_answer_agrees_with_ranking(&metadata, &ranked) {
                return Ok(Some(metadata));
            }
            eprintln!("{}", resolver_disagreement_note(q, &metadata));
        }

        if let Some(metadata) = ranked.first().and_then(|row| row.get("metadata")).cloned() {
            return Ok(Some(metadata));
        }
        if failures.is_empty() {
            Ok(None)
        } else {
            Err(severe_lookup_message(&failures).unwrap_or_else(|| failures.join("\n")))
        }
    }

    async fn metadata_for_query(
        &self,
        query: &str,
        use_live_queries: bool,
    ) -> Result<serde_json::Value, String> {
        match self
            .resolve_query_to_metadata(query, use_live_queries)
            .await?
        {
            Some(metadata) => Ok(metadata),
            None => Err(format!("Could not resolve: {query}")),
        }
    }

    async fn plaintext_citation_units(&self, text: &str) -> Vec<String> {
        let r = self
            .send_read(|| {
                self.request(endpoints::PARSE_CITATIONS, &[])
                    .json(&serde_json::json!({ "text": text }))
            })
            .await;
        let from_api = match r {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                citation_units_from_parse_payload(&body)
            }
            _ => Vec::new(),
        };
        if from_api.is_empty() {
            split_plaintext_citations(text)
        } else {
            from_api
        }
    }

    async fn resolve_plaintext_entries(
        &self,
        units: &[BatchLookupItem],
    ) -> (Vec<serde_json::Value>, Vec<String>) {
        let futs: Vec<_> = units
            .iter()
            .map(|item| {
                let server = self.clone();
                let query = item.text.clone();
                let position = item.index + 1;
                async move {
                    match server.resolve_query_to_metadata(&query, false).await {
                        Ok(Some(metadata)) => Ok(metadata),
                        Ok(None) => Err(format!(
                            "[{position}] Could not resolve: {}",
                            &query[..query.len().min(60)]
                        )),
                        Err(error) => Err(format!("[{position}] {error}")),
                    }
                }
            })
            .collect();
        let resolved: Vec<_> = stream::iter(futs)
            .buffered(read_only_concurrency())
            .collect()
            .await;
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for result in resolved {
            match result {
                Ok(meta) => entries.push(meta),
                Err(e) => errors.push(e),
            }
        }
        errors.sort_by_key(|error| crate::http_error::failure_rank(error));
        (entries, errors)
    }

    async fn format_entries_csl(
        &self,
        entries: &[serde_json::Value],
        style: &str,
        locale: &str,
    ) -> Result<String, String> {
        let fmt = self
            .send_read(|| {
                self.request(endpoints::FORMAT, &[])
                    .json(&serde_json::json!({
                        "entries": entries,
                        "style": style,
                        "locale": locale,
                    }))
            })
            .await;
        match fmt {
            Ok(r) if r.status().is_success() => {
                let result: serde_json::Value = r.json().await.unwrap_or_default();
                let plain = result["plain"].as_str().unwrap_or("").trim();
                if !plain.is_empty() {
                    return Ok(plain.to_string());
                }
                if let Some(fe) = result["entries"].as_array() {
                    let lines: Vec<String> = fe
                        .iter()
                        .map(|entry| entry["bib_plain"].as_str().unwrap_or("").trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !lines.is_empty() {
                        return Ok(lines.join("\n"));
                    }
                }
                Err("Format returned no bibliography text.".into())
            }
            Ok(r) => Err(failure_text(r, "Format failed").await),
            Err(e) => Err(format!("Format failed: {e}")),
        }
    }

    async fn import_plaintext(
        &self,
        collection: Option<&str>,
        content: &str,
        style: Option<&str>,
        locale: &str,
    ) -> String {
        let units = self.plaintext_citation_units(content).await;
        if units.is_empty() {
            return "No citations found in text.".into();
        }
        let has_key = inbound_auth::has_api_key();
        let quota = self.fetch_me_quota().await;
        let pf = plan_metered_batch(
            &units,
            &HashSet::new(),
            &HashMap::new(),
            quota.as_ref(),
            has_key,
        );
        if let Some(msg) = pf.refuse_message {
            return msg;
        }
        let (entries, errors) = self.resolve_plaintext_entries(&pf.need_lookup).await;
        if entries.is_empty() {
            return format!("No citations resolved.\n{}", errors.join("\n"));
        }
        let bib = match render_bibtex_entries(&entries) {
            Ok(bib) => bib,
            Err(error) => return error.to_string(),
        };
        let mut out = format!(
            "Resolved {} of {} citations.\n\n{bib}",
            entries.len(),
            units.len()
        );
        if let Some(style) = style.map(str::trim).filter(|s| !s.is_empty()) {
            match self.format_entries_csl(&entries, style, locale).await {
                Ok(csl) => {
                    out.push_str("\nCSL text:\n");
                    out.push_str(&csl);
                }
                Err(e) => {
                    out.push_str(&format!("\nCSL format skipped: {e}"));
                }
            }
        }
        if let Some(name) = collection {
            if let Some(msg) = block_mutate() {
                return format!("{msg}\n\n{out}");
            }
            let col_id = match self.resolve_or_create_collection(name).await {
                Ok(id) => id,
                Err(e) => return format!("{e}\n\n{out}"),
            };
            let r = self
                .send_read(|| {
                    self.request(endpoints::COLLECTION_ENTRIES_BATCH, &[("id", &col_id)])
                        .json(&serde_json::json!({ "entries": &entries }))
                })
                .await;
            let saved = match r {
                Ok(r) if r.status().is_success() => {
                    let data: serde_json::Value = r.json().await.unwrap_or_default();
                    let added = data["added"].as_u64().unwrap_or(0);
                    let dupes = data["duplicates_skipped"].as_u64().unwrap_or(0);
                    format!("Imported into '{name}': {added} added, {dupes} duplicates skipped")
                }
                Ok(r) if r.status().as_u16() == 401 => inbound_auth::auth_required_text().into(),
                Ok(r) => failure_text(r, "Import failed").await,
                Err(e) => format!("Import failed: {e}"),
            };
            out = format!("{saved}\n\n{out}");
        }
        if !errors.is_empty() {
            out.push_str(&format!("\n\nUnresolved:\n{}", errors.join("\n")));
        }
        out
    }

    // --- Phase 1: High-value new tools ---

    #[tool(
        name = "health_check",
        description = "Check if the OokCite API is reachable and healthy. Returns service status and cache statistics. Call this when another tool fails with a network or server error, to tell an outage apart from a bad request; use doctor instead when authentication or configuration is what looks wrong.",
        annotations(
            title = "API health check",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn health_check(
        &self,
        #[allow(unused)] Parameters(_args): Parameters<HealthCheckArgs>,
    ) -> String {
        let r = self
            .send_read(|| self.request(endpoints::HEALTH, &[]))
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                let status = data["status"].as_str().unwrap_or("unknown");
                let version = data["version"].as_str().unwrap_or("unknown");
                let mut out = format!("Status: {status}\nVersion: {version}");
                if let Some(cache) = data.get("cache") {
                    let hits = cache["hits"].as_u64().unwrap_or(0);
                    let misses = cache["misses"].as_u64().unwrap_or(0);
                    out.push_str(&format!("\nCache: {hits} hits, {misses} misses"));
                }
                // /api/health can read ok while the collection store is
                // refusing. A signed-in probe says so. Anonymous health
                // stays the process check it already was.
                if inbound_auth::has_api_key() {
                    match self
                        .send_read(|| self.request(endpoints::COLLECTIONS_LIST, &[]))
                        .await
                    {
                        Ok(collections) if collections.status().is_success() => {
                            out.push_str("\nCollections: reachable");
                        }
                        Ok(collections) if collections.status().as_u16() == 401 => {}
                        Ok(collections) => {
                            let status = collections.status();
                            out.push_str(&format!("\nCollections: HTTP {status}"));
                        }
                        Err(error) => {
                            out.push_str(&format!("\nCollections: unreachable ({error})"));
                        }
                    }
                }
                out
            }
            Ok(resp) => format!(
                "API unhealthy: HTTP {}\n\n{}",
                resp.status(),
                setup_help_block()
            ),
            Err(e) => format!("API unreachable: {e}\n\n{}", setup_help_block()),
        }
    }

    #[tool(
        name = "doctor",
        description = "Diagnose ookcite-mcp readiness: MCP version, mutate/read-only policy, redacted API key presence, API health, and /me plan when a key is set. Use when setup is unclear or tools fail. Never returns the full API key.",
        annotations(
            title = "ookcite doctor",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn doctor(
        &self,
        #[allow(unused)] Parameters(_args): Parameters<HealthCheckArgs>,
    ) -> String {
        self.doctor_report().await
    }

    #[tool(
        name = "import_bibliography",
        description = "Import bibliography content. Accepts BibTeX, RIS, or a pasted plaintext citation list (format=plaintext or auto). A small plaintext paste (within the anonymous batch cap) returns .bib with no API key and no collection. Passing collection still saves resolved entries when a key is set. BibTeX/RIS file import into a collection requires a key. A BibTeX import on a single-user server (stdio, or HTTP with auth=none) keeps each source entry locally so export_collection can print it unchanged. A shared bearer or OAuth server does not keep that copy. Prefer this over adding entries one at a time.",
        annotations(
            title = "Import bibliography",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn import_bibliography(
        &self,
        Parameters(args): Parameters<ImportBibliographyArgs>,
    ) -> String {
        let kind = detect_bibliography_kind(&args.content, &args.format);
        let collection = optional_collection_name(&args.collection);
        if kind == BibliographyKind::Plaintext {
            return self
                .import_plaintext(
                    collection,
                    &args.content,
                    args.style.as_deref(),
                    &args.locale,
                )
                .await;
        }
        let Some(collection) = collection else {
            return "Collection name required for BibTeX/RIS import. Omit collection only for a small plaintext paste (returns .bib, no API key).".into();
        };
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_or_create_collection(collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let filename = if kind == BibliographyKind::Ris {
            "import.ris"
        } else {
            "import.bib"
        };
        let source_text = (kind == BibliographyKind::Bibtex).then(|| args.content.clone());
        let part = match reqwest::multipart::Part::text(args.content)
            .file_name(filename.to_string())
            .mime_str("text/plain")
        {
            Ok(p) => p,
            Err(_) => return "Failed to construct upload.".into(),
        };
        let form = reqwest::multipart::Form::new().part("file", part);

        let r = self
            .request(endpoints::COLLECTION_IMPORT, &[("id", &col_id)])
            .multipart(form)
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let added = data["added"].as_u64().unwrap_or(0);
                let dupes = data["duplicates_skipped"].as_u64().unwrap_or(0);
                let message = format!(
                    "Imported into '{collection}': {added} added, {dupes} duplicates skipped"
                );
                if let Some(text) = source_text {
                    self.keep_import_source(collection, &col_id, &text, message)
                        .await
                } else {
                    message
                }
            }
            Ok(r) if r.status().as_u16() == 401 => inbound_auth::auth_required_text().into(),
            Ok(r) => failure_text(r, "Import failed").await,
            Err(e) => format!("Import failed: {e}"),
        }
    }

    /// Keep the imported BibTeX on a single-user server so export can print it.
    fn append_kept_sources(
        message: &mut String,
        recorded: &overrides::RecordedSources,
        offered: usize,
    ) {
        message.push_str(&format!(
            " Kept {} source entries for export.",
            recorded.kept
        ));
        if recorded.kept != offered {
            let missed = offered.saturating_sub(recorded.kept);
            message.push_str(&format!(" {missed} imported entries were not stored."));
        }
        for conflict in &recorded.conflicts {
            message.push(' ');
            message.push_str(conflict);
        }
    }

    async fn keep_import_source(
        &self,
        collection: &str,
        col_id: &str,
        content: &str,
        mut message: String,
    ) -> String {
        match &self.overrides {
            overrides::OverrideStore::Disabled => {
                message.push(' ');
                message.push_str(overrides::SHARED_EXPORT_LOSS);
                message
            }
            overrides::OverrideStore::Unavailable => {
                message.push(' ');
                message.push_str(overrides::UNAVAILABLE_MESSAGE);
                message.push_str(" Source entries were not kept.");
                message
            }
            overrides::OverrideStore::Enabled(_) => {
                let parsed = match crate::bib_audit::parse_bibtex_raw(content) {
                    Ok(entries) => entries,
                    Err(error) => {
                        message.push_str(&format!(" Source entries were not kept: {error}."));
                        return message;
                    }
                };
                if parsed.is_empty() {
                    return message;
                }
                let server = match self.fetch_collection_entries(col_id).await {
                    Ok(entries) => entries,
                    Err(error) => {
                        message.push_str(&format!(" Source entries were not kept: {error}."));
                        return message;
                    }
                };
                let paired = match crate::bib_source::pair_with_server(content, &server) {
                    Ok(pairs) => pairs,
                    Err(error) => {
                        message.push_str(&format!(" Source entries were not kept: {error}."));
                        return message;
                    }
                };
                match self.overrides.record_sources(collection, &paired) {
                    Ok(recorded) => {
                        Self::append_kept_sources(&mut message, &recorded, paired.len());
                        message
                    }
                    Err(error) => {
                        message.push_str(&format!(" Source entries were not kept: {error}."));
                        message
                    }
                }
            }
        }
    }

    #[tool(
        name = "check_duplicates",
        description = "Check if a citation already exists in a collection. Resolves the query first, then checks for duplicates. Returns entry_id for matches. Call this before add_to_collection when the user may have saved the work already, and to obtain an entry_id for an entry you only know by citation text.",
        annotations(
            title = "Check collection duplicates",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn check_duplicates(&self, Parameters(args): Parameters<CheckDuplicatesArgs>) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let metadata = match self
            .metadata_for_query(&args.query, args.use_live_queries)
            .await
        {
            Ok(metadata) => metadata,
            Err(error) => return error,
        };

        let r = self
            .send_read(|| {
                self.request(endpoints::COLLECTION_CHECK_DUPLICATES, &[("id", &col_id)])
                    .json(&serde_json::json!({ "metadata": &metadata }))
            })
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let matches: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                if matches.is_empty() {
                    "No duplicates found.".into()
                } else {
                    let mut out = vec![format!("{} potential duplicate(s):", matches.len())];
                    for m in &matches {
                        let match_type = m["match_type"].as_str().unwrap_or("?");
                        let similarity = m["similarity"].as_f64().unwrap_or(0.0);
                        let entry_id = m["entry_id"].as_str().unwrap_or("?");
                        out.push(format!(
                            "- {match_type} ({similarity:.0}%) entry_id: {entry_id}"
                        ));
                    }
                    out.join("\n")
                }
            }
            Ok(r) => failure_text(r, "Duplicate check failed").await,
            Err(error) => format!("Duplicate check failed: {error}"),
        }
    }

    #[tool(
        name = "batch_add_to_collection",
        description = "Add multiple citations to a collection at once. Each query can be a DOI or free-text search. Creates the collection if it doesn't exist, but batch collection workflows may require a paid plan or additional collection capacity. Free-text uses the local corpus by default; set use_live_queries=true to allow live upstream provider calls. Prefer over N× add_to_collection.",
        annotations(
            title = "Batch add to collection",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn batch_add_to_collection(&self, Parameters(args): Parameters<BatchAddArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_or_create_collection(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        // Resolve all queries in parallel (up to 10 concurrent)
        let use_live_queries = args.use_live_queries;
        let futs: Vec<_> = args
            .queries
            .iter()
            .enumerate()
            .map(|(i, query)| {
                let server = self.clone();
                let query = query.clone();
                async move {
                    let q = query.trim();
                    match server.resolve_query_to_metadata(q, use_live_queries).await {
                        Ok(Some(metadata)) => Ok(metadata),
                        Ok(None) => Err(format!(
                            "[{}] Could not resolve: {}",
                            i + 1,
                            &query[..query.len().min(60)]
                        )),
                        Err(error) => Err(format!("[{}] {error}", i + 1)),
                    }
                }
            })
            .collect();
        // Entries land in the collection in this order, and
        // `reorder_collection` exists as a tool because that order is
        // user-visible. `buffered` keeps the order the user gave.
        let resolved: Vec<_> = stream::iter(futs)
            .buffered(MUTATE_BATCH_CONCURRENCY)
            .collect()
            .await;

        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for result in resolved {
            match result {
                Ok(meta) => entries.push(meta),
                Err(e) => errors.push(e),
            }
        }
        errors.sort_by_key(|error| crate::http_error::failure_rank(error));

        if entries.is_empty() {
            return format!("No citations resolved.\n{}", errors.join("\n"));
        }

        // Safe to repeat. The collection store skips an item whose DOI,
        // ISBN, or title-author-year identity is already stored, and each
        // save replaces the whole collection object, so a second POST of
        // this body does not insert that reference again.
        let r = crate::resolve_helpers::send_idempotent_with_retry(|| {
            self.request(endpoints::COLLECTION_ENTRIES_BATCH, &[("id", &col_id)])
                .json(&serde_json::json!({ "entries": &entries }))
        })
        .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let added = data["added"].as_u64().unwrap_or(0);
                let dupes = data["duplicates_skipped"].as_u64().unwrap_or(0);
                let mut out = format!(
                    "Added {added} to '{}', {dupes} duplicates skipped",
                    args.collection
                );
                out.push_str(&batch_add_shortfall_line(
                    entries.len(),
                    added,
                    dupes,
                    data["dropped"].as_u64(),
                ));
                if !errors.is_empty() {
                    out.push_str(&format!("\n\nUnresolved:\n{}", errors.join("\n")));
                }
                out
            }
            Ok(r) => prefixed_failure(r, "Batch add failed").await,
            Err(e) => format!("Batch add failed: {e}"),
        }
    }

    // --- Phase 2: Collection management ---

    #[tool(
        name = "delete_collection",
        description = "Delete a citation collection and every entry in it. This is irreversible and there is no undo. Call this only on an explicit request to delete a collection, and confirm which collection is meant first; to remove a single entry use remove_from_collection instead.",
        annotations(
            title = "Delete collection",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn delete_collection(
        &self,
        Parameters(args): Parameters<DeleteCollectionArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self
            .request(endpoints::COLLECTION_DELETE, &[("id", &col_id)])
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {
                format!("Deleted collection '{}'.", args.collection)
            }
            Ok(r) => prefixed_failure(r, "Failed to delete collection").await,
            Err(e) => format!("Failed to delete collection: {e}"),
        }
    }

    #[tool(
        name = "update_collection",
        description = "Update a collection's name, description, or default citation style. Call this to rename a collection or change the style its entries format in by default; it does not add, remove, or reorder entries.",
        annotations(
            title = "Update collection metadata",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_collection(
        &self,
        Parameters(args): Parameters<UpdateCollectionArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let mut body = serde_json::Map::new();
        if let Some(name) = &args.name {
            body.insert("name".into(), serde_json::json!(name));
        }
        if let Some(desc) = &args.description {
            body.insert("description".into(), serde_json::json!(desc));
        }
        if let Some(style) = &args.default_style {
            body.insert("default_style".into(), serde_json::json!(style));
        }
        if body.is_empty() {
            return "Nothing to update. Provide name, description, or default_style.".into();
        }
        let r = self
            .request(endpoints::COLLECTION_UPDATE, &[("id", &col_id)])
            .json(&serde_json::Value::Object(body))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                format!("Updated collection '{}'.", args.collection)
            }
            Ok(r) => prefixed_failure(r, "Failed to update collection").await,
            Err(e) => format!("Failed to update collection: {e}"),
        }
    }

    #[tool(
        name = "remove_from_collection",
        description = "Remove a specific entry from a collection. Pass entry_id from search_collection, or a bare DOI / doi:10.x/y alias (resolved locally before the API call). Call this to drop one entry while keeping the collection; this changes the collection permanently, so confirm which entry is meant.",
        annotations(
            title = "Remove collection entry",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn remove_from_collection(
        &self,
        Parameters(args): Parameters<RemoveFromCollectionArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let resolved_eid = match self
            .resolve_collection_entry_id(&col_id, &args.entry_id)
            .await
        {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self
            .request(
                endpoints::COLLECTION_ENTRY_REMOVE,
                &[("id", &col_id), ("eid", &resolved_eid)],
            )
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                if r.status().as_u16() == 204 {
                    return format!("Removed entry {resolved_eid} from '{}'.", args.collection);
                }
                let removed: serde_json::Value = r.json().await.unwrap_or_default();
                let entry_id = removed["id"].as_str().unwrap_or(&resolved_eid);
                let title = removed["metadata"]["title"].as_str().unwrap_or("");
                if title.is_empty() {
                    format!("Removed entry {entry_id} from '{}'.", args.collection)
                } else {
                    format!(
                        "Removed entry {entry_id} from '{}': {title}",
                        args.collection
                    )
                }
            }
            Ok(r) => prefixed_failure(r, "Failed to remove entry").await,
            Err(e) => format!("Failed to remove entry: {e}"),
        }
    }

    #[tool(
        name = "update_tags",
        description = "Set tags on a collection. Replaces all existing tags rather than adding to them, so include every tag the collection should keep. Call this when the user wants to label or re-label a collection.",
        annotations(
            title = "Update collection tags",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_tags(&self, Parameters(args): Parameters<UpdateTagsArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self
            .request(endpoints::COLLECTION_TAGS, &[("id", &col_id)])
            .json(&serde_json::json!({"tags": args.tags}))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {
                format!("Updated tags on '{}'.", args.collection)
            }
            Ok(r) => prefixed_failure(r, "Failed to update tags").await,
            Err(e) => format!("Failed to update tags: {e}"),
        }
    }

    #[tool(
        name = "reorder_collection",
        description = "Reorder entries in a collection. Provide the entry IDs in the desired order. Call this when the user wants a specific bibliography order; get the current entry IDs from search_collection first.",
        annotations(
            title = "Reorder collection entries",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn reorder_collection(
        &self,
        Parameters(args): Parameters<ReorderCollectionArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self
            .request(endpoints::COLLECTION_REORDER, &[("id", &col_id)])
            .json(&serde_json::json!({"entry_ids": args.entry_ids}))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {
                format!("Reordered entries in '{}'.", args.collection)
            }
            Ok(r) => prefixed_failure(r, "Failed to reorder collection").await,
            Err(e) => format!("Failed to reorder collection: {e}"),
        }
    }

    // --- Phase 3: Sharing and bulk ops ---

    #[tool(
        name = "share_collection",
        description = "Create a shareable link for a collection. Anyone with the link can view it, so treat it as public. Call this when the user asks to share a collection or send it to someone.",
        annotations(
            title = "Share collection",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn share_collection(&self, Parameters(args): Parameters<ShareCollectionArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self
            .request(endpoints::COLLECTION_SHARE, &[("id", &col_id)])
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let share_url = data["url"].as_str().unwrap_or("?");
                format!("Shared '{}': {share_url}", args.collection)
            }
            Ok(r) => prefixed_failure(r, "Failed to share collection").await,
            Err(e) => format!("Failed to share collection: {e}"),
        }
    }

    #[tool(
        name = "unshare_collection",
        description = "Revoke the shareable link for a collection. Existing links stop working immediately and cannot be restored -- a new share creates a different link. Call this when the user wants to stop sharing.",
        annotations(
            title = "Unshare collection",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn unshare_collection(
        &self,
        Parameters(args): Parameters<UnshareCollectionArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self
            .request(endpoints::COLLECTION_UNSHARE, &[("id", &col_id)])
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {
                format!("Unshared '{}'.", args.collection)
            }
            Ok(r) => prefixed_failure(r, "Failed to unshare collection").await,
            Err(e) => format!("Failed to unshare collection: {e}"),
        }
    }

    #[tool(
        name = "merge_collections",
        description = "Merge multiple collections into one. All entries are combined and duplicates are skipped. Requires an academic or business plan. Call this to consolidate collections; the source collections are not deleted for you.",
        annotations(
            title = "Merge collections",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn merge_collections(
        &self,
        Parameters(args): Parameters<MergeCollectionsArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        if args.collections.len() < 2 {
            return "Need at least 2 collection names to merge.".into();
        }
        // Resolve all collections to full objects
        let cols: Vec<serde_json::Value> =
            match crate::resolve_helpers::send_idempotent_with_retry(|| {
                self.request(endpoints::COLLECTIONS_LIST, &[])
            })
            .await
            {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                Ok(r) => return prefixed_failure(r, "Failed to list collections").await,
                Err(e) => return format!("Failed to list collections: {e}"),
            };

        let mut resolved = Vec::new();
        for name in &args.collections {
            let Some(col) = cols.iter().find(|c| c["name"].as_str() == Some(name)) else {
                return format!("Collection '{name}' not found.");
            };
            // Fetch full collection with entries
            let id = col["id"].as_str().unwrap_or("");
            let r = self
                .send_read(|| self.request(endpoints::COLLECTION_GET, &[("id", id)]))
                .await;
            match r {
                Ok(r) if r.status().is_success() => {
                    let full: serde_json::Value = r.json().await.unwrap_or_default();
                    resolved.push(full);
                }
                Ok(r) => {
                    return format!(
                        "Failed to load collection '{name}': {}",
                        failure_text(r, "request failed").await
                    );
                }
                Err(e) => return format!("Failed to load collection '{name}': {e}"),
            }
        }

        let r = self
            .request(endpoints::COLLECTIONS_MERGE, &[])
            .json(&serde_json::json!({"collections": resolved}))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let merged = data["merged"].as_u64().unwrap_or(0);
                let created = data["created"].as_u64().unwrap_or(0);
                let dupes = data["duplicates_skipped"].as_u64().unwrap_or(0);
                format!("Merged: {merged} entries, {created} new, {dupes} duplicates skipped")
            }
            Ok(r) => prefixed_failure(r, "Merge failed").await,
            Err(e) => format!("Merge failed: {e}"),
        }
    }

    #[tool(
        name = "batch_move_entries",
        description = "Move entries from one collection to another. Requires an academic or business plan. Call this to reorganize entries between existing collections in a single call rather than removing and re-adding them.",
        annotations(
            title = "Batch move entries",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn batch_move_entries(&self, Parameters(args): Parameters<BatchMoveArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let source_id = match self.resolve_collection_id(&args.source).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let target_id = match self.resolve_collection_id(&args.target).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self
            .request(endpoints::COLLECTIONS_BATCH_MOVE, &[])
            .json(&serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "entry_ids": args.entry_ids
            }))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let moved = data["moved"].as_u64().unwrap_or(0);
                format!(
                    "Moved {moved} entries from '{}' to '{}'.",
                    args.source, args.target
                )
            }
            Ok(r) => prefixed_failure(r, "Batch move failed").await,
            Err(e) => format!("Batch move failed: {e}"),
        }
    }

    #[tool(
        name = "view_shared",
        description = "View a shared collection using its share token. Call this when the user supplies a share link or token for a collection they do not own; it needs no API key and does not add anything to the user's own collections.",
        annotations(
            title = "View shared collection",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn view_shared(&self, Parameters(args): Parameters<ViewSharedArgs>) -> String {
        let r = self
            .send_read(|| self.request(endpoints::SHARED_GET, &[("token", &args.share_token)]))
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let col: serde_json::Value = r.json().await.unwrap_or_default();
                let name = col["name"].as_str().unwrap_or("?");
                let entries = col["entries"].as_array().map(|a| a.len()).unwrap_or(0);
                let mut out = vec![format!("Shared collection: {name} ({entries} entries)")];
                if let Some(arr) = col["entries"].as_array() {
                    for e in arr.iter().take(20) {
                        let meta = &e["metadata"];
                        let title = meta["title"].as_str().unwrap_or("?");
                        let authors = meta["authors"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|p| p["family"].as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        let year = meta["date"]["year"]
                            .as_i64()
                            .map(|y| format!(" ({y})"))
                            .unwrap_or_default();
                        out.push(format!("- {authors}{year}: {title}"));
                    }
                    if entries > 20 {
                        out.push(format!("... and {} more", entries - 20));
                    }
                }
                out.join("\n")
            }
            Ok(r) if r.status().as_u16() == 404 => {
                "Shared collection not found or link expired.".into()
            }
            Ok(r) => failure_text(r, "Failed to load shared collection").await,
            Err(e) => format!("Failed to load shared collection: {e}"),
        }
    }

    // --- Utility tools ---

    #[tool(
        name = "generate_citation_keys",
        description = "Generate Better BibTeX-style citation keys (e.g. 'goswami2026') for a list of DOIs. Requires an academic or business plan. Call this when the user needs stable \\cite keys for a LaTeX document and wants them to match the Better BibTeX convention.",
        annotations(
            title = "Generate citation keys",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn generate_citation_keys(
        &self,
        Parameters(args): Parameters<GenerateCitationKeysArgs>,
    ) -> String {
        let entries = match self.resolve_dois_with_preflight(&args.dois).await {
            Ok(e) => e,
            Err(msg) => return msg,
        };

        if entries.is_empty() {
            return "Could not resolve any DOIs.".into();
        }

        let r = self
            .send_read(|| {
                self.request(endpoints::CITATION_KEYS, &[])
                    .json(&serde_json::json!({"entries": &entries}))
            })
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let keys = data["keys"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if keys.is_empty() {
                    "No keys generated.".into()
                } else {
                    keys
                }
            }
            Ok(r) => failure_text(r, "Citation key generation failed").await,
            Err(e) => format!("Citation key generation failed: {e}"),
        }
    }

    #[tool(
        name = "expand_journal",
        description = "Expand a journal abbreviation to its full name (e.g. 'JACS' -> 'Journal of the American Chemical Society'). 16,000+ journals supported. Requires an academic or business plan. Call this when a citation carries an abbreviated journal title and the target style requires the full name.",
        annotations(
            title = "Expand journal abbreviation",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn expand_journal(&self, Parameters(args): Parameters<ExpandJournalArgs>) -> String {
        let r = self
            .send_read(|| {
                self.request(endpoints::JOURNAL_EXPAND, &[])
                    .json(&serde_json::json!({"abbreviation": &args.abbreviation}))
            })
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let found = data["found"].as_bool().unwrap_or(false);
                if found {
                    let full = data["full_name"].as_str().unwrap_or("?");
                    format!("{} -> {full}", args.abbreviation)
                } else {
                    format!("No expansion found for '{}'", args.abbreviation)
                }
            }
            Ok(r) => failure_text(r, "Journal expansion failed").await,
            Err(e) => format!("Journal expansion failed: {e}"),
        }
    }

    // --- Quota, styles, and batch resolve ---

    #[tool(
        name = "usage",
        description = "Report the plan in effect and how much of the lookup quota is left. Call this when the user asks how many lookups they have remaining, or before a large batch to check whether it will fit under the daily cap. Returns the plan name, whether the request authenticated, and daily remaining/limit, plus the monthly allowance on plans that carry one.",
        annotations(
            title = "Show plan and quota usage",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn usage(&self, Parameters(_args): Parameters<UsageArgs>) -> String {
        match self
            .send_read(|| self.request(endpoints::ME_USAGE, &[]))
            .await
        {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                format_usage_report(&data)
            }
            Ok(r) => failure_text(r, "Failed to read usage").await,
            Err(e) => format!("Failed to read usage: {e}"),
        }
    }

    #[tool(
        name = "list_styles",
        description = "List the CSL citation styles this server carries, one page at a time. Call this when the user wants to browse or enumerate what is available rather than find a style they can already name -- search_styles answers a named style faster. Returns style ID, title, and category per entry; page through the full list with offset and limit.",
        annotations(
            title = "List CSL styles",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_styles(&self, Parameters(args): Parameters<ListStylesArgs>) -> String {
        let offset = args.offset.to_string();
        let limit = args.limit.to_string();
        let r = self
            .send_read(|| {
                self.request(endpoints::STYLES_LIST, &[])
                    .query(&[("offset", offset.as_str()), ("limit", limit.as_str())])
            })
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let styles: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                if styles.is_empty() {
                    return format!("No styles at offset {offset}.");
                }
                let mut out = Vec::with_capacity(styles.len() + 1);
                for s in &styles {
                    let id = s["id"].as_str().unwrap_or("?");
                    let title = s["title"].as_str().unwrap_or("?");
                    let category = s["category"].as_str().unwrap_or("Unknown");
                    out.push(format!("ID: {id} | Title: {title} | Category: {category}"));
                }
                out.push(format!(
                    "({} styles from offset {offset}; next page: offset={})",
                    styles.len(),
                    args.offset as u64 + styles.len() as u64
                ));
                out.join("\n")
            }
            Ok(r) => failure_text(r, "Failed to list styles").await,
            Err(e) => format!("Failed to list styles: {e}"),
        }
    }

    #[tool(
        name = "batch_resolve",
        description = "First call for a block of free-text citation strings when you want to know what each one matched without formatting it. Returns, per input, the resolved DOI (when found), the matched title, and the confidence. Always prefer this over N parallel reverse_lookup calls, and over asking Crossref directly: it queries OokCite's ranker once per input, then enriches confident hits from the OokCite/Crossref/DataCite record. batch_format resolves and also renders each in a CSL style. Every input resolves independently so one failure does not abort the rest; the synchronous endpoint accepts at most 50 inputs per call.",
        annotations(
            title = "Batch resolve citations",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn batch_resolve(&self, Parameters(args): Parameters<BatchResolveArgs>) -> String {
        if args.citations.is_empty() {
            return "No citations supplied.".into();
        }
        if args.citations.len() > SYNC_BATCH_RESOLVE_LIMIT {
            return format!(
                "REFUSED: {} citations exceeds the {SYNC_BATCH_RESOLVE_LIMIT}-input limit for a synchronous batch. Split the list and call again.",
                args.citations.len()
            );
        }
        let body = batch_resolve_request_body(&args.citations, args.use_live_queries);
        let r = self
            .send_read(|| self.request(endpoints::RESOLVE_BATCH, &[]).json(&body))
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let payload: serde_json::Value = r.json().await.unwrap_or_default();
                format_batch_resolve_results(&args.citations, &payload).join("\n")
            }
            Ok(r) => failure_text(r, "Batch resolve failed").await,
            Err(e) => format!("Batch resolve failed: {e}"),
        }
    }

    #[tool(
        name = "normalize_bibliography",
        description = "Re-render a BibTeX or RIS bibliography as canonical BibTeX with consistent keys and field order. Requires an academic or business plan. Call this when the user has a messy .bib or .ris file they want cleaned up as text, without saving anything -- import_bibliography is the tool that puts the entries into a collection. Returns the parsed entry count and the rewritten content; nothing is stored server-side.",
        annotations(
            title = "Normalize bibliography",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn normalize_bibliography(
        &self,
        Parameters(args): Parameters<NormalizeBibliographyArgs>,
    ) -> String {
        let r = self
            .send_read(|| {
                self.request(endpoints::BIBLIOGRAPHY_NORMALIZE, &[])
                    .json(&serde_json::json!({
                        "content": &args.content,
                        "format": &args.format,
                    }))
            })
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let count = data["count"].as_u64().unwrap_or(0);
                let content = data["content"].as_str().unwrap_or("");
                format!("Normalized {count} entries.\n\n{content}")
            }
            Ok(r) => failure_text(r, "Normalization failed").await,
            Err(e) => format!("Normalization failed: {e}"),
        }
    }

    #[tool(
        name = "enhanced_search",
        description = "Search the OokCite corpus for a topic, an author's output, or works that match a date, a container, or a DOI prefix, and optionally aggregate the hits by author, category, or citation statistics. Call this before falling back to Crossref or OpenAlex for a related-work watch: created_after and published_after answer \"what is new since T?\" from the local index, container and doi_prefix narrow to a venue or publisher, and sort=date lists the newest first. Pass q=\"*\" when only the filters matter. Returns scored matches plus whichever facets were requested; the facets summarise the whole result set, not just the page returned. When the local index does not yet know a recent paper, the result is a smaller list, not a lie; follow up with a provider resolve for a DOI the user knows is newer than the corpus watermark.",
        annotations(
            title = "Enhanced corpus search",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn enhanced_search(&self, Parameters(args): Parameters<EnhancedSearchArgs>) -> String {
        if args.q.trim().is_empty() {
            return "No query supplied.".into();
        }
        let limit = args.limit.to_string();
        let offset = args.offset.to_string();
        let mut query: Vec<(&str, String)> =
            vec![("q", args.q.clone()), ("limit", limit), ("offset", offset)];
        for (name, on) in [
            ("include_authors", args.include_authors),
            ("include_categories", args.include_categories),
            ("include_citation_stats", args.include_citation_stats),
        ] {
            if on {
                query.push((name, "true".into()));
            }
        }
        let push_filter =
            |query: &mut Vec<(&str, String)>, name: &'static str, value: &Option<String>| {
                if let Some(value) = value
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    query.push((name, value.to_string()));
                }
            };
        push_filter(&mut query, "created_after", &args.created_after);
        push_filter(&mut query, "published_after", &args.published_after);
        push_filter(&mut query, "container", &args.container);
        if let Some(prefix) = args
            .doi_prefix
            .as_deref()
            .map(str::trim)
            .map(|s| s.trim_end_matches('/'))
            .filter(|value| !value.is_empty())
        {
            query.push(("doi_prefix", prefix.to_string()));
        }
        match args
            .sort
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
        {
            Some(ref value) if value == "date" => query.push(("sort", "date".into())),
            Some(ref value) if value == "relevance" || value.is_empty() => {}
            Some(other) => {
                return format!(
                    "CLIENT ERROR: sort must be \"relevance\" or \"date\", not {other:?}."
                );
            }
            None => {}
        }
        let r = self
            .request(endpoints::SEARCH_ENHANCED, &[])
            .query(&query)
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                format_enhanced_search(&data)
            }
            Ok(r) => failure_text(r, "Enhanced search failed").await,
            Err(e) => format!("Enhanced search failed: {e}"),
        }
    }

    // --- ORCID ---

    #[tool(
        name = "orcid_search",
        description = "Find ORCID author profiles by name, affiliation, or ORCID ID. Call this when the user names a researcher and you need their ORCID ID before filtering reverse_lookup by orcid, or when two authors share a surname and you need to tell them apart. Returns ranked profiles with ORCID ID, name, affiliations, work count, and a few of their DOIs; the server answers 503 when the ORCID index is not configured.",
        annotations(
            title = "Search ORCID authors",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn orcid_search(&self, Parameters(args): Parameters<OrcidSearchArgs>) -> String {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(author) = args
            .author
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            query.push(("author", author.to_string()));
        }
        if let Some(aff) = args
            .affiliation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            query.push(("affiliation", aff.to_string()));
        }
        if let Some(orcid) = args
            .orcid
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            query.push(("orcid", orcid.to_string()));
        }
        if query.is_empty() {
            return "Provide at least one of author, affiliation, or orcid.".into();
        }
        if let Some(limit) = args.limit {
            query.push(("limit", limit.to_string()));
        }
        let r = self
            .request(endpoints::ORCID_SEARCH, &[])
            .query(&query)
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let hits = data["hits"].as_array().cloned().unwrap_or_default();
                if hits.is_empty() {
                    return "No ORCID profiles matched.".into();
                }
                hits.iter()
                    .map(format_orcid_hit)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Ok(r) if r.status().as_u16() == 503 => {
                "ORCID author index is not configured on this server.".into()
            }
            Ok(r) => failure_text(r, "ORCID search failed").await,
            Err(e) => format!("ORCID search failed: {e}"),
        }
    }

    #[tool(
        name = "orcid_profile",
        description = "Fetch one ORCID author profile by its ORCID ID. Call this when the user supplies an ORCID ID directly and wants to know whose it is or what that person has published. Returns the name, affiliations, work count, and a sample of DOIs for that ID, or reports that no profile exists for it.",
        annotations(
            title = "Get ORCID profile",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn orcid_profile(&self, Parameters(args): Parameters<OrcidProfileArgs>) -> String {
        let orcid_id = args.orcid_id.trim();
        if orcid_id.is_empty() {
            return "No ORCID ID supplied.".into();
        }
        let r = self
            .request(endpoints::ORCID_PROFILE, &[("orcid_id", orcid_id)])
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let hit: serde_json::Value = r.json().await.unwrap_or_default();
                format_orcid_hit(&hit)
            }
            Ok(r) if r.status().as_u16() == 404 => {
                format!("No ORCID profile found for {orcid_id}.")
            }
            Ok(r) if r.status().as_u16() == 503 => {
                "ORCID author index is not configured on this server.".into()
            }
            Ok(r) => failure_text(r, "ORCID profile lookup failed").await,
            Err(e) => format!("ORCID profile lookup failed: {e}"),
        }
    }

    #[tool(
        name = "ingest_orcid",
        description = "Pull an ORCID profile's publication list into the server's local search index. Call this when a researcher's papers are missing from reverse_lookup or enhanced_search and the user wants them findable there. Returns counts of works fetched, ingested, enriched, and skipped, plus any per-work errors; works already indexed are skipped rather than duplicated, and nothing is added to the user's collections.",
        annotations(
            title = "Ingest ORCID publications",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn ingest_orcid(&self, Parameters(args): Parameters<IngestOrcidArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let orcid = args.orcid.trim();
        if orcid.is_empty() {
            return "No ORCID ID supplied.".into();
        }
        let r = self
            .request(endpoints::INGEST_ORCID, &[])
            .json(&serde_json::json!({
                "orcid": orcid,
                "enrich_via_crossref": args.enrich_via_crossref,
            }))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let count = |key: &str| data[key].as_u64().unwrap_or(0);
                let mut out = vec![format!(
                    "Ingested {orcid}: {} fetched, {} indexed, {} enriched, {} skipped.",
                    count("works_fetched"),
                    count("works_ingested"),
                    count("works_enriched"),
                    count("works_skipped")
                )];
                if let Some(errors) = data["errors"].as_array().filter(|e| !e.is_empty()) {
                    out.push(format!("{} errors:", errors.len()));
                    for e in errors.iter().take(10) {
                        out.push(format!("- {}", e.as_str().unwrap_or("?")));
                    }
                }
                out.join("\n")
            }
            Ok(r) if r.status().as_u16() == 503 => {
                "The index that receives ingested works is not available on this server.".into()
            }
            Ok(r) => failure_text(r, "ORCID ingest failed").await,
            Err(e) => format!("ORCID ingest failed: {e}"),
        }
    }

    // --- Entry-level collection edits ---

    #[tool(
        name = "update_entry_metadata",
        description = "Correct the stored metadata of one collection entry: title, authors, editor, journal, year, volume, issue, pages, publisher, DOI, URL, version, eid, booktitle, entry_type, institution, chapter, number, howpublished, note, or citation_key. An empty string deletes that field. clear lists further field names to remove; a cleared field, year included, is also left out of the export even when Crossref or DataCite supplies it. When this server runs on the caller's machine (stdio, or HTTP with auth=none), every edit is also kept in a local override file for the collection, which export_collection applies, so a field the server does not store still reaches the BibTeX; local_only=true skips the server update. A shared HTTP server with sign-in keeps no local overrides and refuses local_only. A braced author such as \"{IEEE Computer Society}\" is stored as one organisation. Call this when a DOI resolved to the wrong record, or came back with fields missing, and the user wants the saved entry fixed rather than removed and re-added. Fields left unset keep their current values, and the entry's tags, note, and position in the collection are preserved.",
        annotations(
            title = "Edit collection entry metadata",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_entry_metadata(
        &self,
        Parameters(args): Parameters<UpdateEntryMetadataArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        if !entry_metadata_overrides_present(&args) {
            return "Nothing to update. Provide at least one field to set or clear.".into();
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let entries = match self.fetch_collection_entries(&col_id).await {
            Ok(e) => e,
            Err(e) => return e,
        };
        let Some(eid) = resolve_entry_id_in_collection(&entries, &args.entry_id) else {
            return format!(
                "Entry not found in collection for reference '{}'. Use search_collection to list entry_id values (or pass a bare DOI / doi:10.x/y).",
                args.entry_id.trim()
            );
        };
        let Some(current) = entry_metadata_by_id(&entries, &eid) else {
            return format!("Entry {eid} has no stored metadata to edit.");
        };
        let metadata = apply_entry_metadata_overrides(current, &args);
        let local = self.record_local_override(&args, &eid, current);
        if args.local_only {
            return match local {
                LocalOverride::Saved(saved) => {
                    format!("{saved} The server record was not changed.")
                }
                LocalOverride::Off => format!(
                    "local_only was not applied. {} The server record was not changed.",
                    overrides::DISABLED_MESSAGE
                ),
                LocalOverride::Failed(error) => {
                    format!("{error} The server record was not changed.")
                }
            };
        }
        let local_note = match &local {
            LocalOverride::Saved(saved) => format!(" {saved}"),
            LocalOverride::Off => String::new(),
            LocalOverride::Failed(error) => format!(" {error}"),
        };
        let r = self
            .request(
                endpoints::COLLECTION_ENTRY_METADATA,
                &[("id", &col_id), ("eid", &eid)],
            )
            .json(&serde_json::json!({ "metadata": metadata }))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let entry: serde_json::Value = r.json().await.unwrap_or_default();
                let title = entry["metadata"]["title"]
                    .as_str()
                    .or_else(|| metadata["title"].as_str())
                    .unwrap_or("");
                if title.is_empty() {
                    format!("Updated entry {eid} in '{}'.{local_note}", args.collection)
                } else {
                    format!(
                        "Updated entry {eid} in '{}': {title}{local_note}",
                        args.collection
                    )
                }
            }
            Ok(r) => {
                let failure = prefixed_failure(r, "Failed to update entry metadata").await;
                format!("{failure}{local_note}")
            }
            Err(e) => format!("Failed to update entry metadata: {e}{local_note}"),
        }
    }

    #[tool(
        name = "merge_entries",
        description = "Merge two entries of the same collection into one, keeping the entry named by keep_entry_id. Call this when check_duplicates or search_collection shows the same paper saved twice and the user wants a single record. The absorbed entry is deleted permanently -- its non-empty fields fill gaps in the keeper, tags are unioned and notes concatenated -- so confirm which entry should survive before calling.",
        annotations(
            title = "Merge collection entries",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn merge_entries(&self, Parameters(args): Parameters<MergeEntriesArgs>) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let keep = match self
            .resolve_collection_entry_id(&col_id, &args.keep_entry_id)
            .await
        {
            Ok(id) => id,
            Err(e) => return e,
        };
        let absorb = match self
            .resolve_collection_entry_id(&col_id, &args.absorb_entry_id)
            .await
        {
            Ok(id) => id,
            Err(e) => return e,
        };
        if keep == absorb {
            return format!(
                "keep_entry_id and absorb_entry_id both resolve to {keep}; nothing to merge."
            );
        }
        let r = self
            .request(
                endpoints::COLLECTION_ENTRY_MERGE,
                &[("id", &col_id), ("eid", &keep)],
            )
            .json(&serde_json::json!({ "absorb_id": absorb }))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let entry: serde_json::Value = r.json().await.unwrap_or_default();
                let title = entry["metadata"]["title"].as_str().unwrap_or("");
                if title.is_empty() {
                    format!("Merged {absorb} into {keep} in '{}'.", args.collection)
                } else {
                    format!(
                        "Merged {absorb} into {keep} in '{}': {title}",
                        args.collection
                    )
                }
            }
            Ok(r) => prefixed_failure(r, "Failed to merge entries").await,
            Err(e) => format!("Failed to merge entries: {e}"),
        }
    }
}

/// Outcome of keeping an edit locally. Every message is path-free.
enum LocalOverride {
    Saved(String),
    /// This process keeps no local overrides (a shared HTTP server).
    Off,
    Failed(String),
}

impl Server {
    /// Record an edit in the local override file of its collection. The server
    /// does not yet persist every field, so export reads this file back.
    fn record_local_override(
        &self,
        args: &UpdateEntryMetadataArgs,
        entry_id: &str,
        current: &serde_json::Value,
    ) -> LocalOverride {
        if self.overrides == overrides::OverrideStore::Disabled {
            return LocalOverride::Off;
        }
        let doi = current
            .get("doi")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|doi| !doi.is_empty());
        let key = overrides::CollectionOverrides::key_for(entry_id, doi);
        match self.overrides.record(&args.collection, key, args) {
            Ok(()) => LocalOverride::Saved(format!(
                "Saved a local override for {} in collection '{}'.",
                doi.map_or_else(|| format!("entry {entry_id}"), str::to_string),
                args.collection
            )),
            Err(error) => LocalOverride::Failed(format!("Local override not saved: {error}")),
        }
    }
}

/// One agent-facing line for an ORCID author hit.
fn format_orcid_hit(hit: &serde_json::Value) -> String {
    let orcid_id = hit["orcid_id"].as_str().unwrap_or("?");
    let name = hit["credit_name"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            let given = hit["given"].as_str().unwrap_or("");
            let family = hit["family"].as_str().unwrap_or("");
            format!("{given} {family}").trim().to_string()
        });
    let name = if name.is_empty() { "?".into() } else { name };
    let affiliations = hit["affiliations"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .take(3)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    let affiliations = if affiliations.is_empty() {
        "N/A".into()
    } else {
        affiliations
    };
    let works = hit["work_count"].as_u64().unwrap_or(0);
    let dois = hit["dois"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .take(3)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let doi_hint = if dois.is_empty() {
        String::new()
    } else {
        format!(" | sample DOIs: {dois}")
    };
    format!("ORCID: {orcid_id} | {name} | {affiliations} | {works} works{doi_hint}")
}

/// Matches first, then whichever facets the caller asked the API to compute.
fn format_enhanced_search(data: &serde_json::Value) -> String {
    let total = data["total"].as_u64().unwrap_or(0);
    let offset = data["offset"].as_u64().unwrap_or(0);
    let matches = data["matches"].as_array().cloned().unwrap_or_default();
    if matches.is_empty() {
        return format!("No matches (total {total}).");
    }
    let mut out = vec![format!(
        "{} of {total} matches (offset {offset}):",
        matches.len()
    )];
    for (i, m) in matches.iter().enumerate() {
        let meta = &m["metadata"];
        let title = meta["title"].as_str().unwrap_or("?");
        let doi = meta["doi"].as_str().unwrap_or("?");
        let journal = meta["journal"].as_str().unwrap_or("N/A");
        let year = meta["date"]["year"]
            .as_i64()
            .map(|y| format!(" ({y})"))
            .unwrap_or_default();
        let score = m["score"].as_f64().unwrap_or(0.0);
        out.push(format!(
            "{}. [score:{score:.0}] {title}{year} | {journal} | doi:{doi}",
            offset + i as u64 + 1
        ));
    }
    if let Some(authors) = data["authors"].as_array().filter(|a| !a.is_empty()) {
        out.push("Authors:".into());
        for a in authors.iter().take(15) {
            let name = a["name"].as_str().unwrap_or("?");
            let count = a["count"].as_u64().unwrap_or(0);
            let first = a["first_publication_year"].as_i64();
            let last = a["last_publication_year"].as_i64();
            let span = match (first, last) {
                (Some(f), Some(l)) => format!(" | {f}-{l}"),
                _ => String::new(),
            };
            out.push(format!("- {name} | {count} papers{span}"));
        }
    }
    if let Some(categories) = data["categories"].as_array().filter(|c| !c.is_empty()) {
        out.push("Categories:".into());
        for c in categories.iter().take(15) {
            let name = c["name"].as_str().unwrap_or("?");
            let count = c["count"].as_u64().unwrap_or(0);
            out.push(format!("- {name} | {count}"));
        }
    }
    if let Some(stats) = data["citation_stats"].as_object() {
        let total_citations = stats["total_citations"].as_u64().unwrap_or(0);
        let per_year = stats["avg_citations_per_year"].as_f64().unwrap_or(0.0);
        out.push(format!(
            "Citations: {total_citations} total, {per_year:.1} per year on average"
        ));
        if let Some(top) = stats["top_authors"].as_array().filter(|a| !a.is_empty()) {
            let names = top
                .iter()
                .filter_map(|s| s.as_str())
                .take(5)
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!("Top authors: {names}"));
        }
        if let Some(top) = stats["top_journals"].as_array().filter(|j| !j.is_empty()) {
            let names = top
                .iter()
                .filter_map(|s| s.as_str())
                .take(5)
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!("Top journals: {names}"));
        }
    }
    out.join("\n")
}

/// Why the ranking was taken over the resolver's own answer.
///
/// This reaches stderr, which on the hosted server is the operator's log.
/// A citation query is text somebody submitted, often from a reference list
/// that is not published yet, so the line reports its size and the DOI the
/// resolver picked -- which is what a report about this disagreement needs.
fn resolver_disagreement_note(query: &str, resolved: &serde_json::Value) -> String {
    let size = query.chars().count();
    match crate::resolve_helpers::metadata_doi(resolved) {
        Some(doi) => format!(
            "ookcite-mcp: resolver answer {doi} is absent from the ranked candidates \
             for a {size} character query; taking the ranking"
        ),
        None => format!(
            "ookcite-mcp: the resolver answer is absent from the ranked candidates \
             for a {size} character query; taking the ranking"
        ),
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn tool_result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| item.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pull a structured work record out of the text a tool returned and hoist
/// it into `structured_content`. The record travels in-band between two
/// markers so the tool handler can return a single `String` (which is what
/// the `#[tool_router]` macro signs up for) while the server still answers
/// an MCP client that reads `structured_content` directly.
pub(crate) fn promote_structured_marker(result: &mut CallToolResult) {
    let text = tool_result_text(result);
    let Some(begin) = text.find(crate::work_record::STRUCTURED_MARKER_BEGIN) else {
        return;
    };
    let after_begin = begin + crate::work_record::STRUCTURED_MARKER_BEGIN.len();
    let Some(end_relative) = text[after_begin..].find(crate::work_record::STRUCTURED_MARKER_END)
    else {
        return;
    };
    let end = after_begin + end_relative;
    let json_blob = text[after_begin..end].trim();
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_blob) else {
        return;
    };
    let tail_start = end + crate::work_record::STRUCTURED_MARKER_END.len();
    let mut stripped = String::with_capacity(text.len());
    stripped.push_str(text[..begin].trim_end_matches('\n'));
    let tail = text[tail_start..].trim_start_matches('\n');
    if !tail.is_empty() {
        stripped.push('\n');
        stripped.push_str(tail);
    }
    result.content = vec![ContentBlock::text(stripped)];
    // `promote_upstream_failure` might already have written a structured
    // payload (for a throttle or restart). The work record is a success
    // artifact, so an already-set structured payload wins.
    if result.structured_content.is_none() {
        result.structured_content = Some(parsed);
    }
}

/// A throttle, a timeout, or a restart is a tool error. The text stays,
/// and the structured object carries the status and `Retry-After`.
fn promote_upstream_failure(result: &mut CallToolResult) {
    let text = tool_result_text(result);
    if text.trim() == crate::ridley_library::SAVE_INBOX_UNAVAILABLE {
        result.is_error = Some(true);
        result.structured_content = Some(serde_json::json!({
            "kind": "ridley_save_inbox_unavailable",
            "http_status": 404,
            "message": text,
            "retry_after": null,
        }));
        return;
    }
    let Some(report) = upstream_failure_report(&text) else {
        return;
    };
    result.is_error = Some(true);
    result.structured_content = Some(serde_json::json!({
        "kind": report.kind,
        "http_status": report.http_status,
        "message": text,
        "retry_after": report.retry_after,
    }));
}

impl ServerHandler for Server {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let http_key = context
            .extensions
            .get::<http::request::Parts>()
            .map(|parts| inbound_auth::inbound_api_key(&parts.headers));
        let tcc = ToolCallContext::new(self, request, context);
        let called = self
            .in_credential_scope(http_key, self.tool_router.call(tcc))
            .await;
        called.map(|response| match response {
            CallToolResponse::Complete(mut result) => {
                promote_upstream_failure(&mut result);
                promote_structured_marker(&mut result);
                CallToolResponse::Complete(result)
            }
            other => other,
        })
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let mut listed = ListToolsResult::with_all_items(self.tool_router.list_all());
        // `2026-07-28` requires ttlMs and cacheScope on a list result. The tool
        // list is generated with the binary, so a cached copy is already stale.
        if context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28)
        {
            listed = listed.with_ttl_ms(0).with_cache_scope(CacheScope::Public);
        }
        Ok(listed)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }

    fn get_info(&self) -> ServerConfig {
        // `ProtocolVersion::LATEST` is `2026-07-28`, which has no initialize
        // handshake. Negotiation still echoes a supported older version when
        // the client opens with one.
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "ookcite-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_instructions(
                "OokCite citation tools resolve and format citation METADATA: they return structured fields \
                 (title, authors, year, journal, DOI) and formatted bibliography entries. OokCite does NOT fetch PDFs \
                 or paper content. Ridley library tools are different: local search_library and get_item may read \
                 notes, highlights, and indexed full text from the user's configured ridley-core replica. \
                 Resolve citation metadata through these tools rather than from memory or a web search: a \
                 DOI that looks plausible is not evidence the work exists, and validate_doi is what \
                 separates the two. A DOI that exists can still be a different paper: pass year, journal, \
                 volume, and pages with the claim fields, and treat MISMATCH as a failed check. \
                 A manuscript .bib is checked with audit_bibliography, which reports a DOI that is not \
                 registered and a publisher retraction, withdrawal, or expression of concern. \
                 Prefer the batch tools over repeated single calls -- verify_references, batch_format, \
                 batch_add_to_collection, and import_bibliography each take a whole set in one request. \
                 format_citation and a small plaintext import_bibliography (no collection) need no API key. \
                 Collection tools need OOKCITE_API_KEY; merge_collections, batch_move_entries, \
                 generate_citation_keys, and expand_journal additionally need an academic or business plan. \
                 Tools annotated as destructive change or revoke data permanently -- confirm which \
                 collection or entry is meant before calling one.",
            )
    }
}

#[cfg(feature = "demo-mock")]
impl Server {
    /// A server whose OokCite, Crossref, DataCite, Zenodo, doi.org, Unpaywall
    /// and OpenAlex calls all go to one mock base URL over plain http. Built
    /// only with the `demo-mock` feature, for the recorded demo.
    pub fn new_demo(base: String) -> Self {
        let base = base.trim_end_matches('/').to_string();
        let client = || {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("ookcite-mcp: failed to build demo HTTP client")
        };
        Self {
            tool_router: Self::tool_router()
                + Self::ridley_tool_router()
                + discovery::discovery_routers(),
            http: client(),
            provider_http: client(),
            discovery_http: client(),
            discovery: Some(discovery::DiscoveryBases::shared(base.clone())),
            api_base: base.clone(),
            doi_cache: DoiResponseCache::with_default_ttl(),
            providers: Some(ProviderBases::shared(base)),
            overrides: overrides::OverrideStore::Disabled,
            shared_http: false,
        }
    }
}

#[cfg(test)]
impl Server {
    fn new_with_base(api_base: String) -> Self {
        Self {
            tool_router: Self::tool_router()
                + Self::ridley_tool_router()
                + discovery::discovery_routers(),
            http: build_api_client(5, reqwest::header::HeaderMap::new()),
            provider_http: provider_client(None, 5),
            discovery_http: discovery::discovery_client(None, 5),
            discovery: None,
            api_base,
            doi_cache: DoiResponseCache::with_default_ttl(),
            providers: None,
            overrides: overrides::OverrideStore::Disabled,
            shared_http: false,
        }
    }

    fn with_providers(mut self, base: String) -> Self {
        let bases = ProviderBases::shared(base);
        self.provider_http = crate::constants::build_provider_client_for_tests(
            5,
            std::iter::empty::<String>(),
            bases.test_origins(),
        );
        self.providers = Some(bases);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection_entries::{
        format_collection_entry_line, person_from_name, resolve_entry_id_in_collection,
    };
    use crate::constants::version_output;
    use crate::policy::{mutate_block_message, redact_api_key_hint};
    use crate::tool_args::{
        default_style, AuditBibliographyArgs, BatchMoveArgs, BatchResolveArgs, DoiArgs, FormatArgs,
        MergeEntriesArgs, OrcidProfileArgs, OrcidSearchArgs, ReverseArgs, SaveToRidleyArgs,
        UpdateEntryMetadataArgs, UsageArgs, VerifyArgs,
    };

    /// Serializes OOKCITE_API_KEY mutations across parallel tokio tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub(super) struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvGuard {
        pub(super) fn set(key: &'static str, val: &str) -> Self {
            let lock = env_lock();
            let prev = std::env::var(key).ok();
            // SAFETY: held under env_lock for the guard lifetime.
            unsafe { std::env::set_var(key, val) };
            Self {
                key,
                prev,
                _lock: lock,
            }
        }

        pub(super) fn unset(key: &'static str) -> Self {
            let lock = env_lock();
            let prev = std::env::var(key).ok();
            // SAFETY: held under env_lock for the guard lifetime.
            unsafe { std::env::remove_var(key) };
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn doctor_prelude_has_version_not_raw_key() {
        let prelude = Server::doctor_report_sync_prelude();
        assert!(prelude.contains("ookcite-mcp"));
        assert!(!prelude.contains("ookc_"));
        assert!(redact_api_key_hint(Some("ookc_secretkey12")).contains("…"));
        assert!(mutate_block_message().contains("BLOCKED"));
    }

    /// Quota preflight must refuse before any LOOKUP_DOI POST (wiremock count 0).
    #[tokio::test]
    async fn verify_references_refuses_when_quota_insufficient_without_lookup_posts() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authenticated": true,
                "username": "test",
                "plan": "free",
                "lookups_remaining": 1,
                "lookups_limit": 30
            })))
            .expect(1..)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(0..)
            .mount(&mock)
            .await;
        // Any lookup would fail the test via expect(0) — use a separate mock that must not be hit.
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1/x",
                "title": "should not be fetched"
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_testkey_for_preflight");
        let s = test_server(&mock.uri());
        let out = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1/a".into(), "10.1/b".into(), "10.1/c".into()],
                ..Default::default()
            }))
            .await;
        assert!(
            out.contains("REFUSED"),
            "expected REFUSED quota message, got: {out}"
        );
        assert!(
            !out.contains("should not be fetched"),
            "must not have performed metered lookups: {out}"
        );
    }

    /// Collection members are labeled without requiring LOOKUP_DOI when quota is tight
    /// but membership covers part of the batch (remaining 0 → refuse, members still listed).
    #[tokio::test]
    async fn verify_references_lists_collection_members_without_lookup_when_refused() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "free",
                "lookups_remaining": 0,
                "lookups_limit": 30
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "doi:10.1038/187493a0",
                    "metadata": {
                        "doi": "10.1038/187493a0",
                        "title": "Stimulated Optical Radiation in Ruby"
                    }
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_testkey_members");
        let s = test_server(&mock.uri());
        let out = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/187493a0".into(), "10.1/not-in-collection".into()],
                ..Default::default()
            }))
            .await;
        assert!(out.contains("REFUSED"), "got: {out}");
        assert!(
            out.contains("collection") || out.contains("187493a0"),
            "member should appear in output: {out}"
        );
    }

    async fn mount_mixed_member_batch(mock: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "free",
                "lookups_remaining": 2,
                "lookups_limit": 30
            })))
            .expect(1)
            .mount(mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "lab"}
            ])))
            .expect(1)
            .mount(mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "doi:10.5555/member",
                    "metadata": {
                        "doi": "10.5555/member",
                        "title": "Collection Member"
                    }
                }]
            })))
            .expect(1)
            .mount(mock)
            .await;
        for (doi, title) in [
            ("10.5555/first", "First Lookup"),
            ("10.5555/last", "Last Lookup"),
        ] {
            Mock::given(method("POST"))
                .and(path("/api/v1/lookup/doi"))
                .and(body_string_contains(doi))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "doi": doi,
                    "title": title
                })))
                .expect(1)
                .mount(mock)
                .await;
        }
    }

    #[tokio::test]
    async fn mixed_member_metadata_stays_in_the_callers_doi_order() {
        let mock = MockServer::start().await;
        mount_mixed_member_batch(&mock).await;
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_testkey_mixed_order");
        let entries = test_server(&mock.uri())
            .resolve_dois_with_preflight(&[
                "10.5555/first".into(),
                "10.5555/member".into(),
                "10.5555/last".into(),
            ])
            .await
            .expect("mixed batch should resolve");
        let dois: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry["doi"].as_str())
            .collect();
        assert_eq!(dois, ["10.5555/first", "10.5555/member", "10.5555/last"]);
    }

    #[tokio::test]
    async fn verify_references_keeps_mixed_member_lines_in_input_order() {
        let mock = MockServer::start().await;
        mount_mixed_member_batch(&mock).await;
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_testkey_verify_order");
        let output = test_server(&mock.uri())
            .verify_references(Parameters(VerifyArgs {
                dois: vec![
                    "10.5555/first".into(),
                    "10.5555/member".into(),
                    "10.5555/last".into(),
                ],
                ..Default::default()
            }))
            .await;
        let dois: Vec<_> = output
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();
        assert_eq!(
            dois,
            ["10.5555/first", "10.5555/member", "10.5555/last"],
            "{output}"
        );
    }

    #[tokio::test]
    async fn lookup_doi_cache_avoids_second_api_post() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/187493a0",
                "title": "Stimulated Optical Radiation in Ruby",
                "authors": [{"family": "Maiman"}],
                "date": {"year": 1960},
                "journal": "Nature",
                "volume": "187",
                "issue": "4736"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let a = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        let b = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(a.contains("VALID") && a.contains("Ruby"), "first: {a}");
        assert!(b.contains("VALID") && b.contains("Ruby"), "second: {b}");
        // second must not trigger another POST — enforced by mock expect(1)
    }

    #[tokio::test]
    async fn validate_doi_retries_gateway_timeout_before_reporting_timeout() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(504)
                    .insert_header("retry-after", "0")
                    .set_body_string("lookup deadline elapsed"),
            )
            .expect(3)
            .mount(&mock)
            .await;

        let output = test_server(&mock.uri())
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/gateway-timeout".into(),
                ..Default::default()
            }))
            .await;

        assert!(
            output.starts_with("TIMEOUT 10.1038/gateway-timeout : 504 Gateway Timeout:"),
            "{output}"
        );
        assert!(output.contains("(Retry-After: 0)"), "{output}");
        assert!(!output.contains("TEMPORARY ERROR"), "{output}");
    }

    #[tokio::test]
    async fn validate_doi_reports_request_timeout_as_timeout() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(408)
                    .insert_header("retry-after", "0")
                    .set_body_string("request expired upstream"),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let output = test_server(&mock.uri())
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/request-timeout".into(),
                ..Default::default()
            }))
            .await;

        assert!(
            output.starts_with("TIMEOUT 10.1038/request-timeout : 408 Request Timeout:"),
            "{output}"
        );
        assert!(output.contains("(Retry-After: 0)"), "{output}");
        assert!(!output.starts_with("CLIENT ERROR"), "{output}");
    }

    /// This line goes to stderr, which on the hosted server is the operator's
    /// log. A citation query is submitted text, routinely from a reference
    /// list that is still unpublished, and the resolved record carries a
    /// title. Neither belongs in a log; the DOI and a size do.
    #[test]
    fn the_resolver_disagreement_line_keeps_submitted_text_out_of_the_log() {
        let note = resolver_disagreement_note(
            "Smith & Patel, nanoscale thermometry in situ, submitted 2026",
            &serde_json::json!({
                "doi": "10.18653/v1/d19-1261",
                "title": "Nanometre-scale thermometry in a living cell",
            }),
        );
        assert!(note.contains("10.18653/v1/d19-1261"), "{note}");
        assert!(
            !note.contains("nanoscale thermometry"),
            "the query reached the log: {note}"
        );
        assert!(
            !note.contains("Nanometre-scale"),
            "the resolved title reached the log: {note}"
        );
        assert!(
            note.contains("60"),
            "the query size is still reported: {note}"
        );

        let untitled = resolver_disagreement_note("a query", &serde_json::json!({}));
        assert!(untitled.contains("ranked candidates"), "{untitled}");
        assert!(!untitled.contains("a query"), "{untitled}");
    }

    /// The line the operator actually collects is the `eprintln!`, not the
    /// helper above it. Testing the helper alone leaves the call site free to
    /// go back to formatting the query, so hold the call sites too: no
    /// stderr line in this file may interpolate a submitted query.
    #[test]
    fn no_stderr_line_in_this_file_interpolates_a_submitted_query() {
        // The test half quotes the old format string on purpose, so only the
        // production half is in scope.
        let (production, _) = include_str!("server.rs")
            .split_once("\nmod tests {")
            .expect("server.rs has a test module");
        let mut offenders = Vec::new();
        let mut rest = production;
        while let Some(at) = rest.find("eprintln!") {
            let tail = &rest[at..];
            let end = tail.find(");").map_or(tail.len(), |i| i + 2);
            let call = &tail[..end];
            if call.contains("{q:?}") || call.contains("{q}") || call.contains("{query") {
                offenders.push(call.to_string());
            }
            rest = &tail[end..];
        }
        assert!(
            offenders.is_empty(),
            "a stderr line interpolates a submitted query: {offenders:#?}"
        );
    }

    /// A tool call only enters HTTP scope when the request's headers reached
    /// the handler. If they ever do not, a shared server must still refuse
    /// the process key: running that call under the operator's own
    /// OOKCITE_API_KEY would read and spend the operator's account for
    /// whoever sent the request.
    #[tokio::test]
    async fn a_shared_http_call_without_headers_does_not_use_the_process_key() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_operator_key");

        let shared = Server::new().with_shared_http(true);
        let seen = shared
            .in_credential_scope(None, async { inbound_auth::api_key() })
            .await;
        assert_eq!(
            seen, None,
            "a shared server ran a headerless call as the operator"
        );

        let caller = shared
            .in_credential_scope(Some(Some("ookc_caller".into())), async {
                inbound_auth::api_key()
            })
            .await;
        assert_eq!(caller.as_deref(), Some("ookc_caller"));

        let stdio = Server::new();
        let own = stdio
            .in_credential_scope(None, async { inbound_auth::api_key() })
            .await;
        assert_eq!(
            own.as_deref(),
            Some("ookc_operator_key"),
            "a single-user server still uses the key it was given"
        );
    }

    /// The cache is one map for the whole process. Two HTTP requests are two
    /// accounts, so a DOI the first one paid for must still cost the second
    /// one a metered lookup: the API is where quota is counted, and an answer
    /// that never reaches it is a lookup nobody is charged for.
    #[tokio::test]
    async fn an_http_request_is_not_served_from_another_requests_lookup() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/187493a0",
                "title": "Stimulated Optical Radiation in Ruby",
                "authors": [{"family": "Maiman"}],
                "date": {"year": 1960},
                "journal": "Nature"
            })))
            .expect(2)
            .mount(&mock)
            .await;

        let server = test_server(&mock.uri());
        let lookup = |key: &'static str| {
            let server = server.clone();
            async move {
                inbound_auth::with_http_bearer(Some(key.into()), async {
                    server
                        .validate_doi(Parameters(DoiArgs {
                            doi: "10.1038/187493a0".into(),
                            ..Default::default()
                        }))
                        .await
                })
                .await
            }
        };
        let first = lookup("ookc_tenant_one").await;
        let second = lookup("ookc_tenant_two").await;
        assert!(first.contains("VALID"), "first: {first}");
        assert!(second.contains("VALID"), "second: {second}");
        // Two POSTs, one per account — enforced by mock expect(2).
        // The map has no size bound and evicts only on read, so an HTTP
        // lookup that wrote to it would grow it for every account served.
        assert_eq!(
            server.doi_cache.len_for_test(),
            0,
            "an HTTP lookup was written to the process cache"
        );
    }

    /// All DOIs are collection members + remaining=0: batch_format must not POST LOOKUP_DOI.
    #[tokio::test]
    async fn batch_format_collection_members_never_post_lookup_doi() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "free",
                "lookups_remaining": 0,
                "lookups_limit": 30
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    {
                        "id": "doi:10.1038/187493a0",
                        "metadata": {
                            "doi": "10.1038/187493a0",
                            "title": "Stimulated Optical Radiation in Ruby"
                        }
                    },
                    {
                        "id": "doi:10.1/other",
                        "metadata": {
                            "doi": "10.1/other",
                            "title": "Other Member Paper"
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    {"intext_plain": "(Maiman, 1960)", "bib_plain": "Ruby."},
                    {"intext_plain": "(Other, 2020)", "bib_plain": "Other."}
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_batch_format_members");
        let s = test_server(&mock.uri());
        let out = s
            .batch_format(Parameters(crate::tool_args::BatchArgs {
                citations: vec!["10.1038/187493a0".into(), "10.1/other".into()],
                style: default_style(),
                locale: "en-US".into(),
                use_live_queries: false,
            }))
            .await;
        assert!(
            !out.contains("REFUSED"),
            "all members + remaining 0 should proceed without metered need: {out}"
        );
        assert!(
            out.contains("Ruby") || out.contains("Maiman") || out.contains("Other"),
            "format should run on synthetic member meta: {out}"
        );
    }

    #[tokio::test]
    async fn verify_references_formats_collection_members_from_publisher_record() {
        let mock = MockServer::start().await;
        let member_doi = "10.5555/member.paper.1";
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "free",
                "lookups_remaining": 60,
                "lookups_limit": 60
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-member", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-member"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": format!("doi:{member_doi}"),
                    "metadata": {"doi": member_doi, "title": "Short collection title"}
                }]
            })))
            .mount(&mock)
            .await;
        // The DOI is a member: /api/v1/lookup/doi must not be called for it.
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;
        // Member formatting goes directly to the publisher.
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", member_doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "message": {
                    "type": "journal-article",
                    "DOI": member_doi,
                    "title": ["The Full Record Title From Crossref"],
                    "author": [{"family": "Example", "given": "A."}],
                    "container-title": ["Journal of Synthetic Fixtures"],
                    "volume": "4",
                    "issue": "1",
                    "page": "7-21",
                    "publisher": "Example Press",
                    "issued": {"date-parts": [[2026]]}
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_member_verify");
        let out = test_server(&mock.uri())
            .with_providers(mock.uri())
            .verify_references(Parameters(VerifyArgs {
                dois: vec![member_doi.into()],
                claims: Vec::new(),
            }))
            .await;

        assert!(
            out.starts_with(&format!("VALID {member_doi} : ")),
            "member line still starts with VALID: {out}"
        );
        assert!(
            out.contains("The Full Record Title From Crossref"),
            "title must come from the publisher record, not the collection stub: {out}"
        );
        assert!(
            !out.contains("(in your collection)"),
            "authorless stub must not reach the output: {out}"
        );
        assert!(
            out.contains("[collection — not metered]"),
            "member line still names the collection and the free semantic: {out}"
        );
        assert!(
            out.contains("Example, A."),
            "member line must carry the authors from the publisher record: {out}"
        );
    }

    #[tokio::test]
    async fn verify_references_prints_degraded_on_a_publisher_failure_for_a_member() {
        let mock = MockServer::start().await;
        let member_doi = "10.5555/member.degraded.1";
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "free",
                "lookups_remaining": 60,
                "lookups_limit": 60
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-degraded", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-degraded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": format!("doi:{member_doi}"),
                    "metadata": {"doi": member_doi, "title": "Short collection title"}
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;
        // Crossref throttles the publisher read for this DOI: 429 with
        // a Retry-After. DataCite is not tried after a 429, since 429
        // is not a 404.
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", member_doi)))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_string("slow down"),
            )
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_member_degraded");
        let out = test_server(&mock.uri())
            .with_providers(mock.uri())
            .verify_references(Parameters(VerifyArgs {
                dois: vec![member_doi.into()],
                claims: Vec::new(),
            }))
            .await;

        assert!(
            out.starts_with(&format!("DEGRADED {member_doi} : ")),
            "a publisher throttle must not pass as VALID: {out}"
        );
        assert!(!out.contains("VALID"), "{out}");
        assert!(
            out.contains("publisher read failed"),
            "DEGRADED line must name the cause so an agent can retry: {out}"
        );
    }

    #[tokio::test]
    async fn batch_format_keeps_mixed_member_and_lookup_entries_in_input_order() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "free",
                "lookups_remaining": 10,
                "lookups_limit": 60
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "doi:10.1/member",
                    "metadata": {"doi": "10.1/member", "title": "Member paper"}
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains("10.1/live"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1/live",
                "title": "Live paper"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .and(body_json(serde_json::json!({
                "entries": [
                    {"doi": "10.1/live", "title": "Live paper"},
                    {
                        "doi": "10.1/member",
                        "title": "Member paper",
                        "entry_type": "article",
                        "authors": []
                    }
                ],
                "style": "apa",
                "locale": "en-US"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [
                    {"intext_plain": "[1]", "bib_plain": "Live paper."},
                    {"intext_plain": "[2]", "bib_plain": "Member paper."}
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_batch_format_order");
        let result = test_server(&mock.uri())
            .batch_format(Parameters(crate::tool_args::BatchArgs {
                citations: vec!["10.1/live".into(), "10.1/member".into()],
                style: default_style(),
                locale: "en-US".into(),
                use_live_queries: false,
            }))
            .await;
        assert!(
            result.starts_with("[1] Live paper.\n[2] Member paper."),
            "{result}"
        );
    }

    #[test]
    fn test_endpoint_url_construction() {
        let u = endpoints::LOOKUP_DOI.url("https://example.com", &[]);
        assert_eq!(u, "https://example.com/api/v1/lookup/doi");
    }

    #[test]
    fn test_endpoint_url_with_path_params() {
        let u = endpoints::COLLECTION_ENTRIES_ADD.url("https://example.com", &[("id", "abc-123")]);
        assert_eq!(u, "https://example.com/api/v1/collections/abc-123/entries");
    }

    #[test]
    fn version_output_includes_current_version() {
        assert_eq!(
            version_output(),
            format!("ookcite-mcp {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn get_info_instructions_cover_batch_and_destructive_guidance() {
        let info = Server::new().get_info();
        let instr = info.instructions.expect("server instructions set");
        assert!(instr.contains("METADATA"), "scope must be stated");
        assert!(
            instr.contains("does NOT fetch PDFs"),
            "the boundary a client cannot infer from the tool list must be stated"
        );
        assert!(
            instr.contains("batch") && instr.contains("verify_references"),
            "batch preference must survive, named concretely"
        );
        assert!(
            instr.contains("destructive"),
            "destructive-tool caution must survive"
        );
        assert_eq!(info.server_info.name, "ookcite-mcp");
        assert_eq!(info.protocol_version.as_str(), "2026-07-28");
        assert!(info.capabilities.tools.is_some());
    }

    /// The instructions field rides in every request. Enumerating each tool's
    /// trigger there duplicates the tool list the client already has, so the
    /// triggers live in the tools' own descriptions instead; this guards the
    /// enumeration from creeping back one line at a time.
    #[test]
    fn get_info_instructions_do_not_reenumerate_the_tool_list() {
        let info = Server::new().get_info();
        let instr = info.instructions.expect("server instructions set");
        for phrase in [
            "use validate_doi",
            "use lookup_isbn",
            "use reverse_lookup",
            "use delete_collection",
            "use share_collection",
            "use expand_journal",
        ] {
            assert!(
                !instr.contains(phrase),
                "instructions re-enumerate the tool list ({phrase}); \
                 put the trigger in that tool's own description instead"
            );
        }
        assert!(
            instr.len() < 1500,
            "instructions grew to {} chars; it is paid on every request",
            instr.len()
        );
    }

    #[test]
    fn test_default_style() {
        assert_eq!(default_style(), "apa");
    }

    #[test]
    fn test_default_bibtex() {
        assert_eq!(default_bibtex(), "bibtex");
    }

    #[tokio::test]
    async fn test_error_detail_json_message() {
        let resp = http::Response::builder()
            .status(403)
            .header("content-type", "application/json")
            .body(r#"{"error":"plan_required","message":"This feature requires academic plan."}"#)
            .unwrap();
        let resp = reqwest::Response::from(resp);
        let detail = error_detail(resp).await;
        assert_eq!(
            detail,
            "403 Forbidden: This feature requires academic plan."
        );
    }

    #[tokio::test]
    async fn test_error_detail_plain_text() {
        let resp = http::Response::builder()
            .status(429)
            .body("Rate limited")
            .unwrap();
        let resp = reqwest::Response::from(resp);
        let detail = error_detail(resp).await;
        assert_eq!(detail, "429 Too Many Requests: Rate limited");
    }

    #[tokio::test]
    async fn test_error_detail_empty_body() {
        let resp = http::Response::builder().status(500).body("").unwrap();
        let resp = reqwest::Response::from(resp);
        let detail = error_detail(resp).await;
        assert_eq!(detail, "500 Internal Server Error");
    }

    #[tokio::test]
    async fn test_error_detail_long_body_truncated() {
        let long = "x".repeat(200);
        let resp = http::Response::builder().status(502).body(long).unwrap();
        let resp = reqwest::Response::from(resp);
        let detail = error_detail(resp).await;
        assert!(detail.starts_with("502 Bad Gateway: "));
        assert!(detail.len() < 160);
    }

    #[test]
    fn test_args_doi() {
        let args: DoiArgs = serde_json::from_str(r#"{"doi": "10.1038/187493a0"}"#).unwrap();
        assert_eq!(args.doi, "10.1038/187493a0");
    }

    #[test]
    fn test_args_format_default_style() {
        let args: FormatArgs = serde_json::from_str(r#"{"doi": "10.1038/187493a0"}"#).unwrap();
        assert_eq!(args.style, "apa");
    }

    #[test]
    fn test_args_format_custom_style() {
        let args: FormatArgs =
            serde_json::from_str(r#"{"doi": "10.1038/187493a0", "style": "ieee"}"#).unwrap();
        assert_eq!(args.style, "ieee");
    }

    #[test]
    fn test_args_import_default_format() {
        let args: ImportBibliographyArgs =
            serde_json::from_str(r#"{"collection": "test", "content": "@article{...}"}"#).unwrap();
        assert_eq!(args.format, "auto");
        assert_eq!(args.collection.as_deref(), Some("test"));
    }

    #[test]
    fn test_args_import_plaintext_omits_collection() {
        let args: ImportBibliographyArgs = serde_json::from_str(
            r#"{"content": "1. Maiman Nature 1960\n2. Einstein 1905", "format": "plaintext"}"#,
        )
        .unwrap();
        assert!(args.collection.is_none());
        assert_eq!(args.format, "plaintext");
    }

    #[test]
    fn test_args_import_ris() {
        let args: ImportBibliographyArgs = serde_json::from_str(
            r#"{"collection": "test", "content": "TY - JOUR", "format": "ris"}"#,
        )
        .unwrap();
        assert_eq!(args.format, "ris");
    }

    #[test]
    fn test_args_export_defaults_to_bib() {
        let args: ExportCollectionArgs = serde_json::from_str(r#"{"collection": "lab"}"#).unwrap();
        assert_eq!(args.format, "bib");
        assert_eq!(args.style, "apa");
        assert_eq!(args.software_entry, "software");
        assert!(!args.enrich);
    }

    #[test]
    fn test_args_batch_add() {
        let args: BatchAddArgs = serde_json::from_str(
            r#"{"collection": "refs", "queries": ["10.1038/187493a0", "Einstein 1905"]}"#,
        )
        .unwrap();
        assert_eq!(args.queries.len(), 2);
    }

    #[test]
    fn test_args_update_collection_optional() {
        let args: UpdateCollectionArgs = serde_json::from_str(r#"{"collection": "refs"}"#).unwrap();
        assert!(args.name.is_none());
        assert!(args.description.is_none());
        assert!(args.default_style.is_none());
    }

    #[test]
    fn test_args_merge() {
        let args: MergeCollectionsArgs =
            serde_json::from_str(r#"{"collections": ["a", "b", "c"]}"#).unwrap();
        assert_eq!(args.collections.len(), 3);
    }

    #[test]
    fn test_args_batch_move() {
        let args: BatchMoveArgs =
            serde_json::from_str(r#"{"source": "a", "target": "b", "entry_ids": ["e1", "e2"]}"#)
                .unwrap();
        assert_eq!(args.source, "a");
        assert_eq!(args.entry_ids.len(), 2);
    }

    // --- Wiremock integration tests ---

    use rmcp::handler::server::wrapper::Parameters;
    use wiremock::matchers::{body_json, body_string_contains, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_server(base: &str) -> Server {
        Server::new_with_base(base.to_string())
    }

    #[tokio::test]
    async fn save_to_ridley_posts_csl_without_a_user_id() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/ridley/libraries/01ARZ3NDEKTSV4RRFFQ69G5FAV/inbox",
            ))
            .and(body_string_contains(r#""source_ref":"10.1000/abc""#))
            .and(body_string_contains(r#""idempotency_key":"ookcite:once""#))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                !body.contains("user_id") && !body.contains("key_b64")
            })
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "01BX5ZZKBKACTAV9WEVGEMMVRZ",
                "status": "pending"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = test_server(&mock.uri());
        let text = server
            .save_to_ridley(Parameters(SaveToRidleyArgs {
                library_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                csl: serde_json::json!({
                    "type": "article-journal",
                    "title": "A paper",
                    "DOI": "10.1000/abc"
                }),
                source_ref: "10.1000/abc".into(),
                idempotency_key: "ookcite:once".into(),
            }))
            .await;
        assert!(text.contains("Queued for Ridley"), "{text}");
        assert!(text.contains("pending"), "{text}");
    }

    #[tokio::test]
    async fn save_to_ridley_uses_the_shared_unavailable_inbox_error() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/ridley/libraries/01ARZ3NDEKTSV4RRFFQ69G5FAV/inbox",
            ))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({ "error": "not_found" })),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .save_to_ridley(Parameters(SaveToRidleyArgs {
                library_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                csl: serde_json::json!({ "type": "article-journal", "title": "A paper" }),
                source_ref: "10.1000/abc".into(),
                idempotency_key: "ookcite:missing".into(),
            }))
            .await;
        assert_eq!(text, crate::ridley_library::SAVE_INBOX_UNAVAILABLE);

        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["kind"], "ridley_save_inbox_unavailable");
        assert_eq!(structured["http_status"], 404);
    }

    #[tokio::test]
    async fn debug_resolve_sends_tagged_text_input() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve/debug"))
            .and(body_string_contains(r#""kind":"text""#))
            .and(body_string_contains(r#""text":"Amrita Goswami""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "cleaned_query": "Amrita Goswami",
                "search_query": "Amrita Goswami",
                "weight_source": "built_in",
                "final_response": {
                    "paper": {
                        "title": "Universal Nucleation Behavior of Sheared Systems",
                        "doi": "10.1103/physrevlett.126.195702"
                    }
                },
                "backends": []
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .debug_resolve(Parameters(DebugResolveArgs {
                text: "Amrita Goswami".into(),
            }))
            .await;

        assert!(
            result.contains("DOI: 10.1103/physrevlett.126.195702"),
            "unexpected debug response: {result}"
        );
    }

    #[tokio::test]
    async fn debug_resolve_formats_final_candidate_list() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve/debug"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "cleaned_query": "Amrita Goswami",
                "search_query": "Amrita Goswami",
                "weight_source": "file:/data/resolver-weights.json",
                "final_response": {
                    "match_type": "candidate_list",
                    "verification": {
                        "status": "ambiguous",
                        "confidence": 0.7025
                    },
                    "candidates": [{
                        "metadata": {
                            "title": "Universal Nucleation Behavior of Sheared Systems",
                            "doi": "10.1103/physrevlett.126.195702",
                            "date": {"year": 2021},
                            "journal": "Physical Review Letters"
                        },
                        "score": 55.18
                    }]
                },
                "backends": []
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .debug_resolve(Parameters(DebugResolveArgs {
                text: "Amrita Goswami".into(),
            }))
            .await;

        assert!(
            result.contains("Universal Nucleation Behavior of Sheared Systems"),
            "unexpected debug response: {result}"
        );
        assert!(
            result.contains("doi:10.1103/physrevlett.126.195702"),
            "unexpected debug response: {result}"
        );
        assert!(
            result.contains("score:55.18"),
            "unexpected debug response: {result}"
        );
        assert!(
            result.contains("Verification: ambiguous"),
            "unexpected debug response: {result}"
        );
        assert!(
            !result.contains("No match found"),
            "unexpected debug response: {result}"
        );
    }

    #[tokio::test]
    async fn test_validate_doi_success() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/187493a0",
                "title": "Stimulated Optical Radiation in Ruby",
                "authors": [{"family": "Maiman", "given": "T. H."}],
                "date": {"year": 1960},
                "journal": "Nature",
                "volume": "187",
                "issue": "4736"
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(result.starts_with("VALID"));
        assert!(result.contains("Stimulated Optical Radiation in Ruby"));
        assert!(result.contains("Maiman"));
    }

    #[tokio::test]
    async fn validate_doi_embeds_a_structured_record_with_per_field_provenance() {
        let mock = MockServer::start().await;
        let doi = "10.5555/structured.corpus.1";
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .mount(&mock)
            .await;
        // Corpus has part of the record (title, authors, year, doi).
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "title": "A paper the corpus knows",
                "authors": [{"family": "Example", "given": "A."}],
                "date": {"year": 2026}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        // Crossref adds volume, pages, journal, publisher, URL, and type.
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "message": {
                    "type": "journal-article",
                    "DOI": doi,
                    "title": ["A paper the corpus knows"],
                    "container-title": ["Journal of Synthetic Fixtures"],
                    "author": [{
                        "family": "Example",
                        "given": "A.",
                        "ORCID": "https://orcid.org/0000-0001-0000-0000"
                    }],
                    "volume": "7",
                    "page": "1-10",
                    "publisher": "Example Press",
                    "URL": "https://doi.org/10.5555/structured.corpus.1",
                    "issued": {"date-parts": [[2026]]}
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = test_server(&mock.uri()).with_providers(mock.uri());
        let text = server
            .validate_doi(Parameters(DoiArgs {
                doi: doi.into(),
                ..Default::default()
            }))
            .await;

        // The marker is still in the raw string the handler returned: it
        // is promote_structured_marker in call_tool that strips it. Here
        // we parse it directly.
        let begin = crate::work_record::STRUCTURED_MARKER_BEGIN;
        let end = crate::work_record::STRUCTURED_MARKER_END;
        let start = text
            .find(begin)
            .unwrap_or_else(|| panic!("no begin marker: {text}"));
        let after_begin = start + begin.len();
        let end_offset = text[after_begin..]
            .find(end)
            .unwrap_or_else(|| panic!("no end marker: {text}"));
        let json_slice = text[after_begin..after_begin + end_offset].trim();
        let record: serde_json::Value =
            serde_json::from_str(json_slice).expect("record is valid JSON");

        assert_eq!(record["kind"], crate::work_record::WORK_RECORD_KIND);
        assert_eq!(record["doi"], doi);
        assert_eq!(record["source"], "ookcite_corpus");
        assert_eq!(record["confidence"], "high");
        assert_eq!(record["from_cache"], false);
        assert!(record["summary"]
            .as_str()
            .unwrap_or("")
            .starts_with("VALID\n"));
        // Title and authors are in both the corpus and the provider, so the
        // provenance tag names both.
        assert_eq!(
            record["fields"]["title"]["provenance"],
            "ookcite_corpus+crossref"
        );
        assert_eq!(
            record["fields"]["authors"]["provenance"],
            "ookcite_corpus+crossref"
        );
        // Volume, pages, publisher only came from Crossref.
        assert_eq!(record["fields"]["volume"]["provenance"], "crossref");
        assert_eq!(record["fields"]["pages"]["provenance"], "crossref");
        assert_eq!(record["fields"]["publisher"]["provenance"], "crossref");
        assert_eq!(record["fields"]["volume"]["value"], "7");
        // The summary is also carried next to the record so a client can
        // read the prose head without parsing content[0].
        assert!(
            record["summary"]
                .as_str()
                .unwrap_or("")
                .contains("Metadata: live"),
            "{record:#}"
        );
    }

    #[tokio::test]
    async fn validate_doi_not_in_corpus_tags_fields_with_the_publisher() {
        let mock = MockServer::start().await;
        let doi = "10.5555/structured.publisher.1";
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "message": {
                    "type": "journal-article",
                    "DOI": doi,
                    "title": ["Only at the publisher"],
                    "container-title": ["Journal of Synthetic Fixtures"],
                    "author": [{"family": "Jones", "given": "B."}],
                    "publisher": "Example Press",
                    "issued": {"date-parts": [[2026]]}
                }
            })))
            .mount(&mock)
            .await;

        let server = test_server(&mock.uri()).with_providers(mock.uri());
        let text = server
            .validate_doi(Parameters(DoiArgs {
                doi: doi.into(),
                ..Default::default()
            }))
            .await;

        let begin = crate::work_record::STRUCTURED_MARKER_BEGIN;
        let end = crate::work_record::STRUCTURED_MARKER_END;
        let start = text.find(begin).unwrap();
        let after = start + begin.len();
        let end_offset = text[after..].find(end).unwrap();
        let record: serde_json::Value =
            serde_json::from_str(text[after..after + end_offset].trim()).unwrap();

        assert_eq!(record["source"], "not_in_corpus");
        assert_eq!(record["fields"]["title"]["provenance"], "crossref");
        assert_eq!(record["fields"]["publisher"]["provenance"], "crossref");
        assert_eq!(record["fields"]["year"]["provenance"], "crossref");
        assert_eq!(record["fields"]["title"]["value"], "Only at the publisher");
    }

    fn extract_structured(text: &str) -> Option<serde_json::Value> {
        let begin = crate::work_record::STRUCTURED_MARKER_BEGIN;
        let end = crate::work_record::STRUCTURED_MARKER_END;
        let start = text.find(begin)?;
        let after_begin = start + begin.len();
        let end_offset = text[after_begin..].find(end)?;
        serde_json::from_str(text[after_begin..after_begin + end_offset].trim()).ok()
    }

    #[test]
    fn validate_doi_tool_listing_advertises_the_work_record_output_schema() {
        // An agent that wants the structured record without parsing
        // the summary prose needs to find the schema on the tool
        // listing. If the tool macro stops carrying it, downstream
        // generators fall back to "unknown output shape".
        let server = Server::new();
        let tools = server.tool_router.list_all();
        let validate = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "validate_doi")
            .expect("validate_doi is listed");
        let schema = validate
            .output_schema
            .as_ref()
            .expect("validate_doi must advertise an output schema");
        let kind = schema
            .get("properties")
            .and_then(|value| value.get("kind"))
            .and_then(|value| value.get("const"))
            .and_then(|value| value.as_str());
        assert_eq!(kind, Some(crate::work_record::WORK_RECORD_KIND));
    }

    #[tokio::test]
    async fn validate_doi_cache_hit_keeps_provider_and_corpus_provenance_tags() {
        // The first call retrieves the corpus row and overlays the
        // Crossref record; the overlay is kept in the cache. The
        // second call must rebuild the structured record from the
        // cached pre-merge row and cached provider record, so the
        // field provenance ("ookcite_corpus+crossref", "crossref")
        // and author ORCIDs are the same across the two calls.
        let mock = MockServer::start().await;
        let doi = "10.5555/cache.provenance.1";
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "title": "A cached paper",
                "authors": [{"family": "Keeper", "given": "K."}],
                "date": {"year": 2026}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "message": {
                    "DOI": doi,
                    "title": ["A cached paper"],
                    "author": [{
                        "family": "Keeper", "given": "K.",
                        "ORCID": "https://orcid.org/0000-0003-5555-0002"
                    }],
                    "container-title": ["Fixtures Weekly"],
                    "issued": {"date-parts": [[2026]]},
                    "type": "journal-article"
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = test_server(&mock.uri()).with_providers(mock.uri());

        let first = server
            .validate_doi(Parameters(DoiArgs {
                doi: doi.into(),
                ..Default::default()
            }))
            .await;
        let first_record = extract_structured(&first).expect("first call has a record");
        assert_eq!(
            first_record["fields"]["title"]["provenance"],
            "ookcite_corpus+crossref"
        );
        let first_orcid = first_record["authors"][0]["orcid"].as_str().unwrap();
        assert_eq!(first_orcid, "https://orcid.org/0000-0003-5555-0002");

        let second = server
            .validate_doi(Parameters(DoiArgs {
                doi: doi.into(),
                ..Default::default()
            }))
            .await;
        assert!(
            second.contains("Metadata: cached"),
            "second call must be a cache hit: {second}"
        );
        let second_record = extract_structured(&second).expect("cache hit has a record");
        assert_eq!(
            second_record["fields"]["title"]["provenance"], "ookcite_corpus+crossref",
            "cache hit must keep the provider+corpus provenance: {second}"
        );
        assert_eq!(
            second_record["authors"][0]["orcid"].as_str(),
            Some("https://orcid.org/0000-0003-5555-0002"),
            "cache hit must keep the ORCID picked up from the provider: {second}"
        );
    }

    #[tokio::test]
    async fn validate_doi_reports_live_then_cached_metadata_age() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/187493a0",
                "title": "Stimulated Optical Radiation in Ruby"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = test_server(&mock.uri());
        let first = server
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(
            first.contains("Metadata: live (retrieved ")
                && first.contains("s ago) from ookcite corpus"),
            "{first}"
        );

        let second = server
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(
            second.contains("Metadata: cached (retrieved ")
                && second.contains("s ago) from ookcite corpus"),
            "{second}"
        );
        assert!(
            !second.contains("published"),
            "freshness must not be inferred from bibliographic dates: {second}"
        );
    }

    #[tokio::test]
    async fn test_validate_doi_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.9999/fake".into(),
                ..Default::default()
            }))
            .await;
        assert!(result.starts_with("INVALID"));
        assert!(!result.contains("CrossRef"));
    }

    #[tokio::test]
    async fn validate_doi_falls_through_to_publisher_when_corpus_has_no_entry() {
        let mock = MockServer::start().await;
        let doi = "10.5555/corpus.gap.1";
        // doi.org says the handle is registered.
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .expect(1)
            .mount(&mock)
            .await;
        // OokCite's corpus has no entry for this DOI.
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock)
            .await;
        // Crossref holds the work.
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "message": {
                    "type": "journal-article",
                    "DOI": doi,
                    "title": ["A publisher-only work the corpus has not ingested"],
                    "container-title": ["Journal of Synthetic Fixtures"],
                    "author": [{"family": "Example", "given": "A."}],
                    "volume": "3",
                    "issue": "2",
                    "page": "101-110",
                    "publisher": "Example Press",
                    "URL": "https://doi.org/10.5555/corpus.gap.1",
                    "issued": {"date-parts": [[2026]]}
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .validate_doi(Parameters(DoiArgs {
                doi: doi.into(),
                ..Default::default()
            }))
            .await;

        assert!(
            result.starts_with("NOT-IN-CORPUS\n"),
            "head must say the corpus has no entry, not INVALID: {result}"
        );
        assert!(!result.contains("INVALID"), "{result}");
        assert!(
            result.contains("A publisher-only work the corpus has not ingested"),
            "{result}"
        );
        assert!(result.contains("Volume: 3"), "{result}");
        assert!(result.contains("Pages: 101-110"), "{result}");
        assert!(
            result.contains("from publisher record (not in OokCite corpus"),
            "freshness line must name the publisher origin: {result}"
        );
    }

    #[tokio::test]
    async fn validate_doi_reports_provider_throttle_rather_than_invalid_on_corpus_miss() {
        let mock = MockServer::start().await;
        let doi = "10.5555/throttle.1";
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", doi)))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "47")
                    .set_body_string("slow down"),
            )
            .mount(&mock)
            .await;
        // DataCite is tried only when Crossref says 404.
        Mock::given(method("GET"))
            .and(path(doi_http_path("/dois", doi)))
            .respond_with(ResponseTemplate::new(404))
            .expect(0)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .validate_doi(Parameters(DoiArgs {
                doi: doi.into(),
                ..Default::default()
            }))
            .await;

        assert!(
            !result.contains("INVALID"),
            "a provider throttle is not an INVALID DOI: {result}"
        );
        assert!(
            !result.contains("NOT-IN-CORPUS"),
            "a throttle is not a corpus-gap verdict: {result}"
        );
        assert!(
            result.contains("RATE LIMITED") || result.contains("429"),
            "throttle must reach the caller: {result}"
        );
    }

    #[tokio::test]
    async fn test_validate_doi_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(429).set_body_string(format!(
                "Daily limit reached ({}/day). Resets in 3h 45m.",
                crate::constants::FREE_DAILY_LOOKUPS
            )))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED"));
        assert!(result.contains("Daily limit"));
        assert!(!result.contains("not found"));
    }

    #[tokio::test]
    async fn test_validate_doi_plan_required() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": "plan_required",
                "message": "This feature requires an Academic (EUR 4/mo) or Business (EUR 10/mo) plan.",
                "upgrade_url": "https://my.turtletech.us"
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(result.starts_with("ACCESS DENIED"));
        assert!(result.contains("Academic (EUR 4/mo)"));
        assert!(result.contains("Business (EUR 10/mo)"));
    }

    #[tokio::test]
    async fn test_validate_doi_unauthorized_is_not_reported_as_invalid() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "message": "Authentication required"
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(result.starts_with("ACCESS DENIED"));
        assert!(!result.contains("hallucination"));
        assert!(!result.contains("INVALID"));
    }

    #[tokio::test]
    async fn test_verify_references_preserves_client_error_status() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(422).set_body_string("malformed doi payload"))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/187493a0".into()],
                ..Default::default()
            }))
            .await;

        assert!(result.contains("CLIENT ERROR 10.1038/187493a0"));
        assert!(!result.contains("INVALID 10.1038/187493a0"));
    }

    #[tokio::test]
    async fn test_validate_doi_temporary_upstream_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "0")
                    .set_body_string(
                        "Lookup service temporarily unavailable. Please try again shortly.",
                    ),
            )
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
                ..Default::default()
            }))
            .await;
        assert!(result.starts_with("TEMPORARY ERROR"));
        assert!(!result.contains("INVALID"));
    }

    #[tokio::test]
    async fn enhanced_search_passes_the_new_filters_and_sort_as_query_parameters() {
        use wiremock::matchers::query_param;
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search/enhanced"))
            .and(query_param("q", "*"))
            .and(query_param("limit", "10"))
            .and(query_param("offset", "0"))
            .and(query_param("created_after", "2026-10-01"))
            .and(query_param("published_after", "2026-09-15"))
            .and(query_param("container", "Nature"))
            .and(query_param("doi_prefix", "10.1038"))
            .and(query_param("sort", "date"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total": 2,
                "offset": 0,
                "matches": [
                    {
                        "score": 42.0,
                        "metadata": {
                            "title": "A newer Nature paper",
                            "doi": "10.5555/nature.new.1",
                            "journal": "Nature",
                            "date": {"year": 2026}
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .enhanced_search(Parameters(crate::tool_args::EnhancedSearchArgs {
                q: "*".into(),
                limit: 10,
                offset: 0,
                include_authors: false,
                include_categories: false,
                include_citation_stats: false,
                created_after: Some("2026-10-01".into()),
                published_after: Some("2026-09-15".into()),
                container: Some("Nature".into()),
                doi_prefix: Some("10.1038/".into()),
                sort: Some("date".into()),
            }))
            .await;

        assert!(text.contains("A newer Nature paper"), "{text}");
        assert!(text.contains("doi:10.5555/nature.new.1"), "{text}");
    }

    #[tokio::test]
    async fn enhanced_search_omits_empty_filters_and_normalises_sort() {
        use wiremock::matchers::query_param;
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search/enhanced"))
            .and(query_param("q", "neb"))
            .and(query_param("limit", "20"))
            .and(|request: &wiremock::Request| {
                let pairs: Vec<_> = request.url.query_pairs().collect();
                // No sort parameter is sent for "relevance" and no empty
                // filters creep through.
                !pairs.iter().any(|(key, value)| {
                    (key == "created_after" && value.is_empty())
                        || (key == "published_after" && value.is_empty())
                        || (key == "container" && value.is_empty())
                        || (key == "doi_prefix" && value.is_empty())
                        || key == "sort"
                })
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total": 0, "offset": 0, "matches": []
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .enhanced_search(Parameters(crate::tool_args::EnhancedSearchArgs {
                q: "neb".into(),
                limit: 20,
                offset: 0,
                include_authors: false,
                include_categories: false,
                include_citation_stats: false,
                created_after: Some("   ".into()),
                published_after: None,
                container: Some(String::new()),
                doi_prefix: None,
                sort: Some("relevance".into()),
            }))
            .await;

        assert!(text.contains("No matches"), "{text}");
    }

    #[tokio::test]
    async fn enhanced_search_refuses_an_unknown_sort_before_making_a_request() {
        let mock = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .enhanced_search(Parameters(crate::tool_args::EnhancedSearchArgs {
                q: "topic".into(),
                limit: 20,
                offset: 0,
                include_authors: false,
                include_categories: false,
                include_citation_stats: false,
                created_after: None,
                published_after: None,
                container: None,
                doi_prefix: None,
                sort: Some("popularity".into()),
            }))
            .await;

        assert!(text.starts_with("CLIENT ERROR:"), "{text}");
        assert!(text.contains("\"popularity\""), "{text}");
    }

    #[tokio::test]
    async fn test_reverse_lookup_success() {
        let mock = MockServer::start().await;
        // reverse_lookup prefers /api/v1/reverse (bare array of candidates).
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "metadata": {
                        "title": "Stimulated Optical Radiation in Ruby",
                        "doi": "10.1038/187493a0",
                        "journal": "Nature",
                        "authors": [{"family": "Maiman", "given": "T. H."}]
                    },
                    "score": 95.0
                }
            ])))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Maiman 1960 ruby laser".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;
        assert!(result.contains("Stimulated Optical Radiation"));
        assert!(result.contains("10.1038/187493a0"));
        assert!(
            result.contains("original: Maiman 1960 ruby laser"),
            "original query missing: {result}"
        );
        assert!(
            result.contains("[confidence:95]"),
            "confidence missing: {result}"
        );
        assert!(
            result.contains("title: Stimulated Optical Radiation in Ruby"),
            "title field missing: {result}"
        );
    }

    #[tokio::test]
    async fn plaintext_import_returns_bib_without_api_key() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/parse-citations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "citations": [
                    {"index": 0, "source_text": "1. Maiman 1960", "cleaned_text": "Maiman 1960 ruby"}
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "paper": {
                    "title": "Stimulated Optical Radiation in Ruby",
                    "doi": "10.1038/187493a0",
                    "journal": "Nature",
                    "date": {"year": 1960},
                    "authors": [{"family": "Maiman", "given": "T. H."}]
                },
                "candidates": []
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "score": 95.0,
                    "metadata": {
                        "title": "Stimulated Optical Radiation in Ruby",
                        "doi": "10.1038/187493a0"
                    }
                }
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(401))
            .expect(0)
            .mount(&mock)
            .await;

        let _no_key = EnvGuard::unset("OOKCITE_API_KEY");
        let s = test_server(&mock.uri());
        let result = s
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: None,
                content: "1. Maiman, T. H. Stimulated Optical Radiation in Ruby. Nature (1960)."
                    .into(),
                format: "plaintext".into(),
                style: None,
            }))
            .await;
        assert!(
            result.contains("@article{maimanStimulatedOpticalRadiation1960,"),
            "expected bibtex, got: {result}"
        );
        assert!(result.contains("doi = {10.1038/187493a0}"));
        assert!(
            !result.contains("Authentication required"),
            "small plaintext import must not demand a key: {result}"
        );
    }

    #[tokio::test]
    async fn plaintext_import_refuses_anonymous_over_soft_cap() {
        let mock = MockServer::start().await;
        let over = usize::try_from(crate::constants::ANON_BATCH_SOFT_CAP).unwrap() + 1;
        let citations: Vec<serde_json::Value> = (0..over)
            .map(|i| {
                serde_json::json!({
                    "index": i,
                    "cleaned_text": format!("Paper {i} 2020")
                })
            })
            .collect();
        Mock::given(method("POST"))
            .and(path("/api/v1/parse-citations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "citations": citations
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;

        let _no_key = EnvGuard::unset("OOKCITE_API_KEY");
        let s = test_server(&mock.uri());
        let list = (0..over)
            .map(|i| format!("{}. Paper {i} 2020", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let result = s
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: None,
                content: list,
                format: "plaintext".into(),
                style: None,
            }))
            .await;
        assert!(result.contains("REFUSED"), "got: {result}");
        assert!(result.contains("OOKCITE_API_KEY") || result.contains("anonymous"));
    }

    #[tokio::test]
    async fn export_collection_csl_formats_entries() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "e1",
                    "metadata": {
                        "title": "Stimulated Optical Radiation in Ruby",
                        "doi": "10.1038/187493a0",
                        "authors": [{"family": "Maiman", "given": "T. H."}]
                    }
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plain": "Maiman, T. H. (1960). Stimulated Optical Radiation in Ruby. Nature."
            })))
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_export_csl");
        let s = test_server(&mock.uri());
        let result = s
            .export_collection(Parameters(ExportCollectionArgs {
                locale: "en-US".into(),
                collection: "lab".into(),
                format: "csl".into(),
                style: "apa".into(),
                software_entry: "software".into(),
                eid_as: "eid".into(),
                local_overrides: true,
                enrich: false,
            }))
            .await;
        assert!(
            result.contains("Stimulated Optical Radiation in Ruby"),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn export_collection_bib_uses_the_provider_booktitle() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "e1",
                    "metadata": {
                        "entry_type": "techreport",
                        "title": "How to read floating point numbers accurately",
                        "doi": "10.1145/93542.93557",
                        "journal": "ACM Press",
                        "authors": [{"family": "Clinger", "given": "William D."}],
                        "date": {"year": 1990}
                    }
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1/export.bib"))
            .respond_with(ResponseTemplate::new(200).set_body_string("@techreport{broken,}"))
            .expect(0)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/handles/10.1145%2F93542.93557"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1,
                "handle": "10.1145/93542.93557"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/works/10.1145%2F93542.93557"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "type": "proceedings-article",
                    "title": ["How to read floating point numbers accurately"],
                    "container-title": ["Proceedings of the ACM SIGPLAN 1990 conference on Programming language design and implementation - PLDI '90"],
                    "publisher": "ACM Press",
                    "page": "92-101",
                    "DOI": "10.1145/93542.93557",
                    "URL": "https://doi.org/10.1145/93542.93557",
                    "author": [{"given": "William D.", "family": "Clinger"}],
                    "issued": {"date-parts": [[1990]]}
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_export_bib");
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .export_collection(Parameters(ExportCollectionArgs {
                locale: "en-US".into(),
                collection: "lab".into(),
                format: "bib".into(),
                style: "apa".into(),
                software_entry: "software".into(),
                eid_as: "eid".into(),
                local_overrides: true,
                enrich: false,
            }))
            .await;
        assert!(result.contains("@inproceedings{"), "{result}");
        assert!(result.contains("booktitle = {"), "{result}");
        assert!(result.contains("PLDI"), "{result}");
        assert!(!result.contains("booktitle = {ACM Press}"), "{result}");
        assert!(!result.contains("@techreport"), "{result}");
        assert!(!result.contains("url = {https://doi.org/"), "{result}");
    }

    #[tokio::test]
    async fn export_collection_emits_the_updated_software_version() {
        let mock = MockServer::start().await;
        let doi = "10.5281/zenodo.1000";
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "c-link", "name": "linkcell"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/c-link/import"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .expect(1)
            .mount(&mock)
            .await;
        let entry = |version: &str, publisher: &str| {
            serde_json::json!({
                "id": "c-link",
                "name": "linkcell",
                "entries": [{
                    "id": "e-link",
                    "metadata": {
                        "entry_type": "software",
                        "title": "linkcell",
                        "doi": doi,
                        "version": version,
                        "publisher": publisher,
                        "authors": [{"family": "Goswami", "given": "Rohit"}],
                        "date": {"year": 2024}
                    }
                }]
            })
        };
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/c-link"))
            .respond_with(ResponseTemplate::new(200).set_body_json(entry("0.3.11", "Old Press")))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/c-link"))
            .respond_with(ResponseTemplate::new(200).set_body_json(entry("0.5.0", "Current Press")))
            .mount(&mock)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/collections/c-link/entries/e-link/metadata"))
            .and(body_string_contains("0.5.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "e-link",
                "metadata": {"title": "linkcell", "version": "0.5.0"}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", doi)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/dois", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"attributes": {
                    "doi": doi,
                    "types": {"resourceTypeGeneral": "Software"},
                    "titles": [{"title": "linkcell"}],
                    "creators": [{"name": "Goswami, Rohit", "givenName": "Rohit", "familyName": "Goswami", "nameType": "Personal"}],
                    "publicationYear": 2024,
                    "publisher": "Zenodo",
                    "version": "0.3.11",
                    "relatedIdentifiers": [{"relationType": "IsVersionOf", "relatedIdentifier": "10.5281/zenodo.999"}]
                }}
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/records/1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "conceptdoi": "10.5281/zenodo.999",
                "metadata": {"version": "0.3.11"}
            })))
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_linkcell");
        let server = test_server(&mock.uri()).with_providers(mock.uri());
        let imported = server
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: Some("linkcell".into()),
                content: format!(
                    "@software{{linkcell,\n  title = {{linkcell}},\n  author = {{Goswami, Rohit}},\n  year = {{2024}},\n  version = {{0.3.11}},\n  publisher = {{Old Press}},\n  doi = {{{doi}}}\n}}\n"
                ),
                format: "bibtex".into(),
                style: None,
            }))
            .await;
        assert!(imported.contains("Imported into 'linkcell'"), "{imported}");
        let updated = server
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "linkcell".into(),
                entry_id: "e-link".into(),
                version: Some("0.5.0".into()),
                publisher: Some("Current Press".into()),
                ..Default::default()
            }))
            .await;
        assert!(updated.contains("Updated entry e-link"), "{updated}");
        let bib = server
            .export_collection(Parameters(ExportCollectionArgs {
                locale: "en-US".into(),
                collection: "linkcell".into(),
                format: "bib".into(),
                style: "apa".into(),
                software_entry: "software".into(),
                eid_as: "eid".into(),
                local_overrides: true,
                enrich: false,
            }))
            .await;
        assert!(bib.contains("version = {0.5.0}"), "{bib}");
        assert!(bib.contains("publisher = {Current Press}"), "{bib}");
        assert!(!bib.contains("0.3.11"), "{bib}");
        assert!(!bib.contains("Old Press"), "{bib}");
    }

    #[tokio::test]
    async fn export_collection_bib_rejects_an_unregistered_doi() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "lab"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "e1",
                    "metadata": {
                        "title": "A paper that was never registered",
                        "doi": "10.2307/4615733",
                        "authors": [{"family": "Holm"}]
                    }
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/handles/10.2307%2F4615733"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "responseCode": 100,
                "handle": "10.2307/4615733"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/works/10.2307%2F4615733"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_export_missing");
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .export_collection(Parameters(ExportCollectionArgs {
                locale: "en-US".into(),
                collection: "lab".into(),
                format: "bib".into(),
                style: "apa".into(),
                software_entry: "software".into(),
                eid_as: "eid".into(),
                local_overrides: true,
                enrich: false,
            }))
            .await;
        assert!(result.contains("UNREGISTERED 10.2307/4615733"), "{result}");
        assert!(result.contains("not registered at doi.org"), "{result}");
        assert!(
            !result.contains("@misc") && !result.contains("@article"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn bibtex_import_without_collection_is_refused() {
        let s = test_server("http://127.0.0.1:9");
        let result = s
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: None,
                content: "@article{a,\n  title={T},\n  author={A}\n}".into(),
                format: "bibtex".into(),
                style: None,
            }))
            .await;
        assert!(result.contains("Collection name required"), "got: {result}");
    }

    #[tokio::test]
    async fn test_reverse_lookup_no_matches() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&mock)
            .await;
        // Live fallback resolve also empty.
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "candidates": []
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "nonexistent paper xyz".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;
        assert_eq!(result, "original: nonexistent paper xyz\nNo matches found");
    }

    #[tokio::test]
    async fn reverse_lookup_still_searches_live_when_live_is_forced_and_local_is_empty() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":true"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "score": 80.0,
                    "metadata": {
                        "title": "Attention Is All You Need",
                        "doi": "10.48550/arxiv.1706.03762"
                    }
                }]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Attention Is All You Need".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: true,
            }))
            .await;
        assert!(result.contains("Attention Is All You Need"), "{result}");
        assert!(result.contains("[confidence:80]"), "{result}");
    }

    #[tokio::test]
    async fn reverse_lookup_rejects_an_unregistered_doi() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "score": 90.0,
                    "metadata": {
                        "title": "A JSTOR ghost",
                        "doi": "10.2307/4615733"
                    }
                }])),
            )
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/handles/10.2307%2F4615733"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "responseCode": 100
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Holm 1972 a paper that does not exist".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: true,
            }))
            .await;
        assert!(result.contains("UNREGISTERED 10.2307/4615733"), "{result}");
        assert!(!result.contains("A JSTOR ghost"), "{result}");
    }

    #[tokio::test]
    async fn test_reverse_lookup_falls_back_to_live_when_local_empty() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":true"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": [
                    {
                        "metadata": {
                            "title": "High Throughput Reproducible Literate Phylogenetic Analysis",
                            "doi": "10.1109/pdgc56933.2022.10053210",
                            "journal": "2022 Seventh International Conference on Parallel, Distributed and Grid Computing (PDGC)"
                        },
                        "score": 91.0
                    }
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Goswami Ruhila High Throughput Reproducible Literate Phylogenetic Analysis PDGC 2022".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;

        assert!(result.contains("High Throughput Reproducible Literate Phylogenetic Analysis"));
        assert!(result.contains("10.1109/pdgc56933.2022.10053210"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_falls_back_to_live_when_local_match_is_weak() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "metadata": {
                        "title": "Resource allocation based on redundancy models for high availability cloud",
                        "doi": "10.1007/s00607-019-00728-1",
                        "journal": "Computing"
                    },
                    "score": 2.0
                }
            ])))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":true"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": [
                    {
                        "metadata": {
                            "title": "Attention Is All You Need",
                            "doi": "10.48550/arXiv.1706.03762",
                            "journal": "arXiv"
                        },
                        "score": 88.0
                    }
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Attention Is All You Need Vaswani 2017".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;

        assert!(result.contains("Attention Is All You Need"));
        assert!(result.contains("10.48550/arXiv.1706.03762"));
        assert!(!result.contains("Resource allocation based on redundancy models"));
        // A live score of 88 clears MIN_CONFIDENT_REVERSE_LOOKUP_SCORE
        // (25), so the default path must print the ranked list clean,
        // without the LOW CONFIDENCE header the local score of 2 would
        // have earned on its own.
        assert!(
            !result.contains("LOW CONFIDENCE"),
            "confident live hit was still labelled below the threshold: {result}"
        );
    }

    #[tokio::test]
    async fn test_reverse_lookup_keeps_low_confidence_candidates_with_scores() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "metadata": {
                        "title": "Resource allocation based on redundancy models for high availability cloud",
                        "doi": "10.5555/weak.local.1",
                        "journal": "Computing"
                    },
                    "score": 2.0
                }
            ])))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":true"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": [
                    {
                        "metadata": {
                            "title": "All You Need Is LSD",
                            "doi": "10.5555/live.candidate.1",
                            "journal": "All You Need Is LSD"
                        },
                        "score": 10.0
                    }
                ]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Attention Is All You Need Vaswani 2017".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;

        assert!(
            result.contains("LOW CONFIDENCE"),
            "low-confidence header is missing: {result}"
        );
        assert!(
            result.contains("threshold 25"),
            "threshold must be named so the agent knows the bar: {result}"
        );
        assert!(
            !result.contains("No confident matches found"),
            "near misses must not be erased: {result}"
        );
        // The higher-scoring live candidate replaces the weak local hit.
        assert!(
            result.contains("All You Need Is LSD"),
            "better-scored candidate must be shown: {result}"
        );
        assert!(
            result.contains("10.5555/live.candidate.1"),
            "candidate DOI must be present: {result}"
        );
        assert!(
            result.contains("[confidence:10]"),
            "candidate rank-score must stay visible: {result}"
        );
    }

    #[tokio::test]
    async fn test_reverse_lookup_keeps_the_weak_local_hit_when_live_is_empty() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "metadata": {
                        "title": "A near-miss title the ranker scored low",
                        "doi": "10.5555/weak.local.2",
                        "journal": "Journal of Synthetic Fixtures"
                    },
                    "score": 7.0
                }
            ])))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":true"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": []
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "a messy citation string".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;

        assert!(
            result.contains("LOW CONFIDENCE"),
            "low-confidence header is missing: {result}"
        );
        assert!(
            result.contains("10.5555/weak.local.2"),
            "weak local hit must be preserved when live returns nothing: {result}"
        );
        assert!(
            result.contains("[confidence:7]"),
            "rank-score must stay visible: {result}"
        );
        assert!(
            !result.contains("No confident matches found"),
            "no erasure: {result}"
        );
    }

    #[tokio::test]
    async fn test_health_check_success() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "0.1.0",
                "cache": {"hits": 1234, "misses": 56}
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s.health_check(Parameters(HealthCheckArgs {})).await;
        assert!(result.contains("Status: ok"));
        assert!(result.contains("Version: 0.1.0"));
        assert!(result.contains("1234 hits"));
    }

    #[tokio::test]
    async fn test_health_check_unreachable() {
        let s = test_server("http://127.0.0.1:1");
        let result = s.health_check(Parameters(HealthCheckArgs {})).await;
        assert!(result.starts_with("API unreachable:"));
    }

    #[tokio::test]
    async fn list_collections_outlasts_a_restart_window() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-123", "name": "femtolab", "entry_count": 30}
            ])))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s.list_collections(Parameters(ListCollectionsArgs {})).await;
        assert!(result.contains("femtolab (30 entries)"), "{result}");
    }

    #[tokio::test]
    async fn list_collections_reports_the_reason_a_503_gives() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "0")
                    .set_body_string("Collection storage not configured"),
            )
            .expect(3)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s.list_collections(Parameters(ListCollectionsArgs {})).await;
        assert!(
            result.contains("Collection storage not configured"),
            "{result}"
        );
        assert!(result.contains("after 3 attempts"), "{result}");
    }

    #[tokio::test]
    async fn verify_references_outlasts_a_restart_window() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/restart-window",
                "title": "Recovered After Restart"
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/restart-window".into()],
                ..Default::default()
            }))
            .await;
        assert!(
            result.contains("VALID 10.1038/restart-window : Recovered After Restart"),
            "{result}"
        );
        assert!(!result.contains("TEMPORARY ERROR"), "{result}");
    }

    #[tokio::test]
    async fn verify_references_reports_a_503_after_three_attempts() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "0")
                    .set_body_string("Lookup service temporarily unavailable."),
            )
            .expect(3)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/still-down".into()],
                ..Default::default()
            }))
            .await;
        assert!(
            result.contains("TEMPORARY ERROR 10.1038/still-down"),
            "{result}"
        );
        assert!(
            result.contains("Lookup service temporarily unavailable."),
            "{result}"
        );
        assert!(!result.contains("VALID"), "{result}");
    }

    #[tokio::test]
    async fn batch_add_to_collection_outlasts_a_restart_window() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-restart", "name": "femtolab", "entry_count": 30}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/restart-batch",
                "title": "Stored Once"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-restart/entries/batch"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-restart/entries/batch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .batch_add_to_collection(Parameters(BatchAddArgs {
                collection: "femtolab".into(),
                queries: vec!["10.1038/restart-batch".into()],
                use_live_queries: false,
            }))
            .await;
        assert!(result.contains("Added 1 to 'femtolab'"), "{result}");
        assert!(!result.contains("Batch add failed"), "{result}");
    }

    #[tokio::test]
    async fn batch_add_to_collection_reports_a_503_after_three_attempts() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-restart", "name": "femtolab", "entry_count": 30}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/still-down",
                "title": "Not Stored"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-restart/entries/batch"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "0")
                    .set_body_string("upstream restarting"),
            )
            .expect(3)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .batch_add_to_collection(Parameters(BatchAddArgs {
                collection: "femtolab".into(),
                queries: vec!["10.1038/still-down".into()],
                use_live_queries: false,
            }))
            .await;
        assert!(result.contains("Batch add failed"), "{result}");
        assert!(result.contains("upstream restarting"), "{result}");
        assert!(!result.contains("Added 1"), "{result}");
    }

    #[tokio::test]
    async fn batch_add_to_collection_retries_a_transient_doi_lookup() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-restart", "name": "femtolab", "entry_count": 30}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/lookup-restart",
                "title": "Looked Up After Restart"
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-restart/entries/batch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .batch_add_to_collection(Parameters(BatchAddArgs {
                collection: "femtolab".into(),
                queries: vec!["10.1038/lookup-restart".into()],
                use_live_queries: false,
            }))
            .await;
        assert!(result.contains("Added 1 to 'femtolab'"), "{result}");
        assert!(!result.contains("Could not resolve"), "{result}");
    }

    #[tokio::test]
    async fn batch_add_to_collection_retries_a_transient_resolve() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-restart", "name": "femtolab", "entry_count": 30}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "paper": {
                    "title": "Shifting Balance in Evolution",
                    "doi": "10.1093/genetics/16.2.97"
                }
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-restart/entries/batch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .batch_add_to_collection(Parameters(BatchAddArgs {
                collection: "femtolab".into(),
                queries: vec!["Wright 1931 genetics shifting balance".into()],
                use_live_queries: false,
            }))
            .await;
        assert!(result.contains("Added 1 to 'femtolab'"), "{result}");
        assert!(!result.contains("Could not resolve"), "{result}");
    }

    #[tokio::test]
    async fn test_resolve_collection_id_found() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-123", "name": "My Refs", "entry_count": 5}
            ])))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s.resolve_collection_id("My Refs").await;
        assert_eq!(result, Ok("col-123".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_collection_id_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s.resolve_collection_id("Nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_resolve_collection_id_auth_required() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s.resolve_collection_id("anything").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Authentication required"));
    }

    #[test]
    fn test_resolve_entry_id_accepts_bare_doi_and_doi_prefix() {
        let entries = vec![serde_json::json!({
            "id": "doi:10.1038/nature14539",
            "metadata": {
                "title": "Deep learning",
                "doi": "10.1038/nature14539"
            }
        })];
        assert_eq!(
            resolve_entry_id_in_collection(&entries, "10.1038/nature14539").as_deref(),
            Some("doi:10.1038/nature14539")
        );
        assert_eq!(
            resolve_entry_id_in_collection(&entries, "doi:10.1038/nature14539").as_deref(),
            Some("doi:10.1038/nature14539")
        );
        assert_eq!(
            resolve_entry_id_in_collection(&entries, "DOI:10.1038/NATURE14539").as_deref(),
            Some("doi:10.1038/nature14539")
        );
        assert!(resolve_entry_id_in_collection(&entries, "10.9999/missing").is_none());
    }

    #[test]
    fn test_resolve_entry_id_matches_opaque_id_case_insensitively() {
        let entries = vec![serde_json::json!({
            "id": "entry-ABC",
            "metadata": { "title": "X" }
        })];
        assert_eq!(
            resolve_entry_id_in_collection(&entries, "entry-abc").as_deref(),
            Some("entry-ABC")
        );
    }

    #[test]
    fn test_format_collection_entry_line_includes_entry_id() {
        let entry = serde_json::json!({
            "id": "doi:10.1103/physrevlett.77.3865",
            "metadata": {
                "title": "Generalized Gradient Approximation Made Simple",
                "authors": [{"family": "Perdew"}],
                "date": {"year": 1996},
                "doi": "10.1103/PhysRevLett.77.3865"
            }
        });
        let line = format_collection_entry_line(&entry);
        assert!(line.contains("entry_id: doi:10.1103/physrevlett.77.3865"));
        assert!(line.contains("Perdew"));
        assert!(line.contains("1996"));
        assert!(line.contains("Generalized Gradient Approximation Made Simple"));
        // id is already the doi: form — no redundant aliases clause
        assert!(!line.contains("aliases:"));
    }

    #[test]
    fn test_format_collection_entry_line_hints_doi_alias_for_opaque_id() {
        let entry = serde_json::json!({
            "id": "entry-uuid-1",
            "metadata": {
                "title": "Example",
                "authors": [{"family": "Author"}],
                "date": {"year": 2020},
                "doi": "10.1234/example"
            }
        });
        let line = format_collection_entry_line(&entry);
        assert!(line.contains("entry_id: entry-uuid-1"));
        assert!(line.contains("aliases: doi:10.1234/example"));
    }

    #[tokio::test]
    async fn test_search_collection_returns_stable_entry_ids() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-123", "name": "My Refs", "entry_count": 3}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "col-123",
                "name": "My Refs",
                "entries": [
                    {
                        "id": "entry-good",
                        "metadata": {
                            "title": "Keep This",
                            "authors": [{"family": "Keeper"}],
                            "date": {"year": 2026},
                            "journal": "Examples"
                        }
                    },
                    {
                        "id": "entry-bad",
                        "metadata": {
                            "title": "Quasi-Monte Carlo Methods",
                            "authors": [{"family": "Baldeaux"}],
                            "date": {"year": 2008},
                            "journal": "Monte Carlo Methods and Applications",
                            "doi": "10.1515/mcma.2008.001"
                        }
                    },
                    {
                        "id": "entry-other",
                        "metadata": {
                            "title": "Keep That",
                            "authors": [{"family": "Keeper"}],
                            "date": {"year": 2025},
                            "journal": "Examples"
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .search_collection(Parameters(SearchCollectionArgs {
                collection: "My Refs".into(),
                query: "Quasi-Monte Carlo".into(),
            }))
            .await;

        assert!(result.contains("entry_id: entry-bad"));
        assert!(result.contains("aliases: doi:10.1515/mcma.2008.001"));
        assert!(result.contains("Quasi-Monte Carlo Methods"));
        assert!(!result.contains("entry-good"));
    }

    #[tokio::test]
    async fn test_remove_from_collection_names_removed_citation() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-123", "name": "My Refs", "entry_count": 3}
            ])))
            .mount(&mock)
            .await;
        // remove_from_collection loads the collection first to resolve entry aliases
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "col-123",
                "name": "My Refs",
                "entries": [
                    {
                        "id": "entry-bad",
                        "metadata": {
                            "title": "Quasi-Monte Carlo Methods",
                            "authors": [{"family": "Baldeaux"}]
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/api/v1/collections/col-123/entries/entry-bad"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "entry-bad",
                "metadata": {
                    "title": "Quasi-Monte Carlo Methods",
                    "authors": [{"family": "Baldeaux"}]
                }
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .remove_from_collection(Parameters(RemoveFromCollectionArgs {
                collection: "My Refs".into(),
                entry_id: "entry-bad".into(),
            }))
            .await;

        assert!(result.contains("Removed entry entry-bad"));
        assert!(result.contains("Quasi-Monte Carlo Methods"));
    }

    #[tokio::test]
    async fn test_remove_from_collection_resolves_bare_doi_alias() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-123", "name": "My Refs", "entry_count": 1}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "col-123",
                "name": "My Refs",
                "entries": [
                    {
                        "id": "doi:10.1038/nature14539",
                        "metadata": {
                            "title": "Deep learning",
                            "doi": "10.1038/nature14539"
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("DELETE"))
            .and(path(
                "/api/v1/collections/col-123/entries/doi%3A10.1038%2Fnature14539",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "doi:10.1038/nature14539",
                "metadata": { "title": "Deep learning" }
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .remove_from_collection(Parameters(RemoveFromCollectionArgs {
                collection: "My Refs".into(),
                entry_id: "10.1038/nature14539".into(),
            }))
            .await;

        assert!(result.contains("Removed entry doi:10.1038/nature14539"));
        assert!(result.contains("Deep learning"));
        assert!(!result.contains("Entry not found"));
    }

    #[tokio::test]
    async fn test_remove_from_collection_surfaces_entry_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-123", "name": "My Refs", "entry_count": 3}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/col-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "col-123",
                "name": "My Refs",
                "entries": [
                    {
                        "id": "entry-good",
                        "metadata": { "title": "Keep This" }
                    }
                ]
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .remove_from_collection(Parameters(RemoveFromCollectionArgs {
                collection: "My Refs".into(),
                entry_id: "alias-or-doi".into(),
            }))
            .await;

        assert!(result.contains("Entry not found"));
        assert!(result.contains("alias-or-doi"));
        assert!(!result.contains("Removed entry"));
    }

    #[tokio::test]
    async fn test_resolve_or_create_collection_surfaces_limit_guidance() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "message": "Collection limit reached (1). Purchase additional collections or upgrade your plan."
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s.resolve_or_create_collection("RuhiMastersThesis").await;
        let err = result.expect_err("should fail with guidance");
        assert!(err.contains("Use an existing collection"));
        assert!(err.contains("upgrade your plan"));
    }

    #[tokio::test]
    async fn test_verify_references_parallel() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/187493a0",
                "title": "Test Paper"
            })))
            .expect(3)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/1".into(), "10.1038/2".into(), "10.1038/3".into()],
                ..Default::default()
            }))
            .await;
        assert_eq!(result.lines().count(), 3);
        assert!(result.lines().all(|l| l.starts_with("VALID")));
    }

    #[tokio::test]
    async fn verify_references_compares_a_bibliographic_claim() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1021/j100134a002",
                "title": "Feynman path integral formulation of quantum mechanical transition-state theory",
                "authors": [{"family": "Voth", "given": "Gregory A."}],
                "date": {"year": 1993},
                "journal": "The Journal of Physical Chemistry",
                "volume": "97",
                "issue": "32",
                "pages": "8365-8377"
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1021/j100134a002".into()],
                claims: vec![BibliographicClaim {
                    year: Some(1989),
                    journal: Some("J. Phys. Chem.".into()),
                    volume: Some("93".into()),
                    pages: Some("7009".into()),
                    authors: Some(vec!["Voth".into(), "Chandler".into(), "Miller".into()]),
                    ..BibliographicClaim::default()
                }],
            }))
            .await;
        assert!(result.starts_with("MISMATCH "), "{result}");
        assert!(!result.contains("VALID"), "{result}");
        assert!(
            result.contains("year: claimed 1989, record 1993"),
            "{result}"
        );
        assert!(result.contains("volume: claimed 93, record 97"), "{result}");
    }

    fn doi_http_path(prefix: &str, doi: &str) -> String {
        format!("{prefix}/{}", super::encode_path_segment(doi))
    }

    async fn mount_registered(mock: &MockServer, doi: &str) {
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .expect(1)
            .mount(mock)
            .await;
    }

    async fn mount_doi_lookup(mock: &MockServer, doi: &str, title: &str, year: i32) {
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "title": title,
                "date": {"year": year}
            })))
            .expect(1)
            .mount(mock)
            .await;
    }

    fn crossref_article(
        doi: &str,
        title: &str,
        family: &str,
        year: i32,
        journal: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": "journal-article",
            "DOI": doi,
            "title": [title],
            "author": [{"family": family, "given": "A."}],
            "container-title": [journal],
            "issued": {"date-parts": [[year]]}
        })
    }

    async fn mount_crossref(mock: &MockServer, doi: &str, message: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "message": message
            })))
            .expect(1)
            .mount(mock)
            .await;
    }

    #[tokio::test]
    async fn audit_bibliography_reports_retracted_broken_and_clean_entries() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        let clean = "10.1038/187493a0";
        let retracted = "10.1000/retracted.paper";
        let deposited = "10.1234/datacite.retracted";
        let missing = "10.9999/not-registered";
        let mismatch = "10.1021/j100134a002";
        let concern = "10.1000/concern.paper";

        for doi in [clean, retracted, deposited, mismatch, concern] {
            mount_registered(&mock, doi).await;
        }
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", missing)))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "responseCode": 100
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(missing))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "should not be fetched"
            })))
            .expect(0)
            .mount(&mock)
            .await;

        mount_doi_lookup(&mock, clean, "Stimulated Optical Radiation in Ruby", 1960).await;
        mount_doi_lookup(&mock, retracted, "A result that did not hold", 2018).await;
        mount_doi_lookup(&mock, deposited, "A deposited record", 2019).await;
        mount_doi_lookup(
            &mock,
            mismatch,
            "Feynman path integral formulation of quantum mechanical transition-state theory",
            1993,
        )
        .await;
        mount_doi_lookup(&mock, concern, "A finding under review", 2017).await;

        mount_crossref(
            &mock,
            clean,
            crossref_article(
                clean,
                "Stimulated Optical Radiation in Ruby",
                "Maiman",
                1960,
                "Nature",
            ),
        )
        .await;
        let mut retracted_record = crossref_article(
            retracted,
            "A result that did not hold",
            "Smith",
            2018,
            "Journal of Examples",
        );
        retracted_record["update-to"] = serde_json::json!([{
            "DOI": "10.5555/retraction-notice",
            "type": "retraction",
            "label": "Retraction",
            "updated": {"date-parts": [[2021, 6, 2]]}
        }]);
        mount_crossref(&mock, retracted, retracted_record).await;
        mount_crossref(
            &mock,
            mismatch,
            crossref_article(
                mismatch,
                "Feynman path integral formulation of quantum mechanical transition-state theory",
                "Voth",
                1993,
                "The Journal of Physical Chemistry",
            ),
        )
        .await;
        let mut concern_record =
            crossref_article(concern, "A finding under review", "Adey", 2017, "Nature");
        concern_record["update-to"] = serde_json::json!([{
            "DOI": "10.5555/concern-notice",
            "type": "expression_of_concern",
            "updated": {"date-parts": [[2018, 1, 9]]}
        }]);
        mount_crossref(&mock, concern, concern_record).await;

        Mock::given(method("GET"))
            .and(path(doi_http_path("/dois", deposited)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "attributes": {
                        "doi": deposited,
                        "titles": [{"title": "A deposited record"}],
                        "creators": [{
                            "name": "Lee, Ann",
                            "givenName": "Ann",
                            "familyName": "Lee",
                            "nameType": "Personal"
                        }],
                        "publicationYear": 2019,
                        "types": {"resourceTypeGeneral": "Text"},
                        "relatedIdentifiers": [{
                            "relationType": "IsRetractedBy",
                            "relatedIdentifierType": "DOI",
                            "relatedIdentifier": "https://doi.org/10.1234/datacite.notice"
                        }]
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let bib = r#"
@article{clean,
  title = {Stimulated Optical Radiation in Ruby},
  author = {Maiman, Theodore H.},
  year = {1960},
  journal = {Nature},
  doi = {10.1038/187493a0}
}

@article{pulled,
  title = {A result that did not hold},
  author = {Smith, Jane},
  year = {2018},
  journal = {Journal of Examples},
  doi = {10.1000/retracted.paper}
}

@article{deposited,
  title = {A deposited record},
  author = {Lee, Ann},
  year = {2019},
  doi = {10.1234/datacite.retracted}
}

@article{ghost,
  title = {This DOI was never registered},
  author = {Nobody, N.},
  year = {2020},
  doi = {10.9999/not-registered}
}

@article{wrongyear,
  title = {Feynman path integral formulation of quantum mechanical transition-state theory},
  author = {Voth, Gregory A.},
  year = {1989},
  journal = {J. Phys. Chem.},
  doi = {10.1021/j100134a002}
}

@article{concerned,
  title = {A finding under review},
  author = {Adey, Pat},
  year = {2017},
  journal = {Nature},
  doi = {10.1000/concern.paper}
}

@article{localnote,
  title = {A note with no identifier},
  author = {Author, A.},
  year = {2021}
}
"#;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: bib.into(),
            }))
            .await;

        assert!(
            result.starts_with("Audited 7 entries, 6 with a DOI.\n1 OK, 2 RETRACTED, 1 CONCERN, 1 BROKEN, 1 MISMATCH, 1 NO DOI."),
            "{result}"
        );
        assert!(
            result.contains(
                "clean | OK | doi:10.1038/187493a0 | Stimulated Optical Radiation in Ruby"
            ),
            "{result}"
        );
        assert!(
            result.contains("retracted by doi:10.5555/retraction-notice on 2021-06-02"),
            "{result}"
        );
        assert!(
            result.contains("retracted by doi:10.1234/datacite.notice"),
            "{result}"
        );
        assert!(
            result.contains(
                "ghost | BROKEN | doi:10.9999/not-registered | not registered at doi.org"
            ),
            "{result}"
        );
        assert!(
            result.contains("year: claimed 1989, record 1993"),
            "{result}"
        );
        assert!(
            result.contains("expression of concern doi:10.5555/concern-notice on 2018-01-09"),
            "{result}"
        );
        assert!(
            result.contains("localnote | NO DOI | A note with no identifier"),
            "{result}"
        );
        assert!(
            result.contains("no DOI in the entry; this audit does not guess one"),
            "{result}"
        );
        assert!(
            crate::http_error::upstream_failure_report(&result).is_none(),
            "{result}"
        );
    }

    #[tokio::test]
    async fn audit_bibliography_does_not_call_a_lookup_clean_without_a_publisher_record() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_doi_lookup(
            &mock,
            "10.1038/187493a0",
            "Stimulated Optical Radiation in Ruby",
            1960,
        )
        .await;
        let result = test_server(&mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "@article{clean, title={Stimulated Optical Radiation in Ruby}, year={1960}, doi={10.1038/187493a0}}\n@article{again, doi={10.1038/187493a0}}".into(),
            }))
            .await;
        assert!(
            result.contains("Audited 2 entries, 2 with a DOI."),
            "{result}"
        );
        assert!(result.contains("| UNCHECKED |"), "{result}");
        assert!(
            result.contains("no Crossref or DataCite record was read"),
            "{result}"
        );
        assert!(!result.contains("| OK |"), "{result}");
    }

    #[tokio::test]
    async fn audit_bibliography_refuses_past_the_anonymous_cap_without_a_lookup() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;
        let mut content = String::new();
        for n in 0..=crate::constants::ANON_BATCH_SOFT_CAP {
            content.push_str(&format!("@article{{k{n}, doi={{10.1000/cap{n}}}}}\n"));
        }
        let result = test_server(&mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs { content }))
            .await;
        assert!(result.starts_with("REFUSED:"), "{result}");
        assert!(result.contains("does not skip this audit"), "{result}");
        assert!(crate::http_error::upstream_failure_report(&result).is_some());
    }

    #[tokio::test]
    async fn audit_bibliography_refuses_when_the_signed_in_quota_is_spent() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_audit_quota");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authenticated": true,
                "plan": "free",
                "lookups_remaining": 0,
                "lookups_limit": 60
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;
        let result = test_server(&mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "@article{one, doi={10.1038/187493a0}}".into(),
            }))
            .await;
        assert!(result.contains("only 0 remaining"), "{result}");
        assert!(result.contains("plan free limit 60/day"), "{result}");
    }

    #[tokio::test]
    async fn audit_bibliography_reports_a_lookup_rate_limit_as_upstream() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_registered(&mock, "10.1038/187493a0").await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_string("Daily limit reached."),
            )
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/works", "10.1038/187493a0")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "@article{slow, title={Ruby}, doi={10.1038/187493a0}}".into(),
            }))
            .await;
        assert!(result.contains("| UPSTREAM |"), "{result}");
        assert!(result.contains("RATE LIMITED 10.1038/187493a0"), "{result}");
        assert!(!result.contains("| OK |"), "{result}");
        assert!(crate::http_error::upstream_failure_report(&result).is_some());
    }

    #[test]
    fn provider_redirect_allowlist_is_the_fixed_hosts() {
        let hosts: HashSet<_> = ProviderBases::production().hosts().into_iter().collect();
        assert_eq!(
            hosts,
            HashSet::from([
                "api.crossref.org".to_string(),
                "api.datacite.org".to_string(),
                "zenodo.org".to_string(),
                "doi.org".to_string(),
            ])
        );
    }

    #[tokio::test]
    async fn audit_bibliography_treats_a_success_without_response_code_as_upstream_never_ok_or_broken(
    ) {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        let doi = "10.1000/no-code";
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "handle": doi
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: format!("@article{{plain, title={{Left}}, doi={{{doi}}}}}"),
            }))
            .await;
        assert!(
            result.contains("plain | UPSTREAM | doi:10.1000/no-code")
                && result.contains("not a registration verdict: it has no responseCode"),
            "{result}"
        );
        assert!(
            !result.contains("BROKEN") && !result.contains("| OK |"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn audit_bibliography_refuses_an_off_host_provider_redirect() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let origin = MockServer::start().await;
        let other = MockServer::start().await;
        let doi = "10.1000/redirected";
        let location = format!(
            "{}{}",
            other.uri().replacen("127.0.0.1", "localhost", 1),
            doi_http_path("/handles", doi)
        );
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(302).insert_header("location", location.as_str()))
            .expect(1)
            .mount(&origin)
            .await;
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .expect(0)
            .mount(&other)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&origin)
            .await;
        let result = test_server(&origin.uri())
            .with_providers(origin.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: format!("@article{{hop, doi={{{doi}}}}}"),
            }))
            .await;
        assert!(result.contains("provider redirect refused"), "{result}");
        assert!(!result.contains("| OK |"), "{result}");
    }

    #[tokio::test]
    async fn audit_bibliography_refuses_an_oversized_provider_body() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        let doi = "10.1000/huge";
        let body = vec![b'x'; crate::constants::PROVIDER_BODY_LIMIT + 1];
        Mock::given(method("GET"))
            .and(path(doi_http_path("/handles", doi)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: format!("@article{{huge, doi={{{doi}}}}}"),
            }))
            .await;
        assert!(
            result.contains(&format!(
                "exceeded {} bytes",
                crate::constants::PROVIDER_BODY_LIMIT
            )),
            "{result}"
        );
        assert!(!result.contains("| OK |"), "{result}");
    }

    async fn mount_no_provider_calls(mock: &MockServer) {
        Mock::given(method("GET"))
            .and(path_regex("^/handles/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "responseCode": 1
            })))
            .expect(0)
            .mount(mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(mock)
            .await;
    }

    fn cap_bibliography() -> String {
        let mut content = String::new();
        for n in 0..=crate::constants::ANON_BATCH_SOFT_CAP {
            content.push_str(&format!("@article{{k{n}, doi={{10.1000/cap{n}}}}}\n"));
        }
        content
    }

    #[tokio::test]
    async fn audit_bibliography_keeps_the_cap_when_quota_is_not_confirmed() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_unconfirmed");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authenticated": true,
                "plan": "free"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        mount_no_provider_calls(&mock).await;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: cap_bibliography(),
            }))
            .await;
        assert!(result.contains("quota was not confirmed"), "{result}");
        assert!(
            result.contains(&format!(
                "soft cap {}",
                crate::constants::ANON_BATCH_SOFT_CAP
            )),
            "{result}"
        );
    }

    #[tokio::test]
    async fn audit_bibliography_keeps_the_cap_when_me_fails() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_me_down");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .expect(1)
            .mount(&mock)
            .await;
        mount_no_provider_calls(&mock).await;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: cap_bibliography(),
            }))
            .await;
        assert!(result.contains("quota was not confirmed"), "{result}");
        assert!(!result.contains("doi.org"), "{result}");
    }

    #[tokio::test]
    async fn audit_bibliography_rejects_a_bad_key_before_doi_org() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_rejected");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(401).set_body_string("no"))
            .expect(1)
            .mount(&mock)
            .await;
        mount_no_provider_calls(&mock).await;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "@article{one, doi={10.1038/187493a0}}".into(),
            }))
            .await;
        assert!(result.starts_with("ACCESS DENIED"), "{result}");
        assert!(result.contains("did not call doi.org"), "{result}");
    }

    #[tokio::test]
    async fn audit_bibliography_does_not_call_doi_org_when_the_key_cannot_be_checked() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_unchecked");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .expect(1)
            .mount(&mock)
            .await;
        mount_no_provider_calls(&mock).await;
        let result = test_server(&mock.uri())
            .with_providers(mock.uri())
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "@article{one, doi={10.1038/187493a0}}".into(),
            }))
            .await;
        assert!(
            result.starts_with("REFUSED: the API key could not be checked"),
            "{result}"
        );
        assert!(result.contains("did not call doi.org"), "{result}");
    }

    #[tokio::test]
    async fn audit_bibliography_rejects_an_empty_file_and_ris() {
        let server = test_server("http://127.0.0.1:9");
        let empty = server
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "   \n".into(),
            }))
            .await;
        assert_eq!(empty, "No BibTeX entries found.");
        let ris = server
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "TY  - JOUR\nTI  - Hello\nER  - \n".into(),
            }))
            .await;
        assert!(ris.contains("looks like RIS"), "{ris}");
        let broken = server
            .audit_bibliography(Parameters(AuditBibliographyArgs {
                content: "@article{x, title={nope".into(),
            }))
            .await;
        assert!(broken.starts_with("BibTeX parse error:"), "{broken}");
    }

    #[tokio::test]
    async fn test_verify_references_retries_transient_lookup_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains("10.1038/retry"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "0")
                    .set_body_string("Lookup service temporarily unavailable."),
            )
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains("10.1038/retry"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/retry",
                "title": "Recovered Paper"
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/retry".into()],
                ..Default::default()
            }))
            .await;

        assert!(result.contains("VALID 10.1038/retry : Recovered Paper"));
        assert!(!result.contains("TEMPORARY ERROR"));
    }

    #[tokio::test]
    async fn test_verify_references_preserves_rate_limit_status() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(429).set_body_string(format!(
                "Daily limit reached ({}/day). Resets in 5h.",
                crate::constants::FREE_DAILY_LOOKUPS
            )))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/187493a0".into()],
                ..Default::default()
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED 10.1038/187493a0 :"));
        assert!(!result.contains("INVALID"));
    }

    #[tokio::test]
    async fn batch_format_stops_after_exact_doi_rate_limit() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "academic",
                "lookups_remaining": 100,
                "lookups_limit": 1000
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains("10.9999/mcp-rate-limit-contract"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Daily limit reached"))
            .expect(1)
            .mount(&mock)
            .await;

        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_batch_rate_limit_contract");
        let s = test_server(&mock.uri());
        let result = s
            .batch_format(Parameters(crate::tool_args::BatchArgs {
                citations: vec!["10.9999/mcp-rate-limit-contract".into()],
                style: default_style(),
                locale: "en-US".into(),
                use_live_queries: true,
            }))
            .await;

        assert!(result.contains("RATE LIMITED 10.9999/mcp-rate-limit-contract"));
        assert!(!result.contains("Not found"));
    }

    #[tokio::test]
    async fn test_verify_references_preserves_mixed_statuses() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains("10.1038/good"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/good",
                "title": "Good Paper"
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains("10.1038/slow"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "0")
                    .set_body_string("Lookup service temporarily unavailable."),
            )
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/good".into(), "10.1038/slow".into()],
                ..Default::default()
            }))
            .await;

        assert!(result.contains("VALID 10.1038/good : Good Paper"));
        assert!(result.contains("TEMPORARY ERROR 10.1038/slow :"));
        assert!(!result.contains("INVALID 10.1038/slow"));
    }

    #[tokio::test]
    async fn test_resolve_query_to_metadata_prefers_resolve_paper() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "paper": {
                    "title": "Shifting Balance in Evolution",
                    "doi": "10.1093/genetics/16.2.97"
                }
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let metadata = s
            .resolve_query_to_metadata("Wright 1931 genetics shifting balance", false)
            .await
            .expect("resolve")
            .expect("metadata");

        assert_eq!(metadata["doi"].as_str(), Some("10.1093/genetics/16.2.97"));
        assert_eq!(
            metadata["title"].as_str(),
            Some("Shifting Balance in Evolution")
        );
    }

    #[tokio::test]
    async fn test_resolve_query_to_metadata_accepts_verified_candidate_list() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "verification": { "status": "verified", "confidence": 0.97 },
                "candidates": [
                    {
                        "metadata": {
                            "title": "EVOLUTION IN MENDELIAN POPULATIONS",
                            "doi": "10.1093/genetics/16.2.97"
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            // The ranker is consulted on every free-text resolve now, so the
            // resolver's answer can be checked against a ranked candidate
            // set instead of being taken on its own word. An empty ranking
            // cannot overrule it, so the outcome below is unchanged.
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let metadata = s
            .resolve_query_to_metadata("Wright 1931 genetics shifting balance", false)
            .await
            .expect("resolve")
            .expect("metadata");

        assert_eq!(metadata["doi"].as_str(), Some("10.1093/genetics/16.2.97"));
    }

    #[tokio::test]
    async fn test_add_to_collection_accepts_verified_candidate_list() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "My References", "entry_count": 0}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "verification": { "status": "verified", "confidence": 0.97 },
                "candidates": [
                    {
                        "metadata": {
                            "title": "EVOLUTION IN MENDELIAN POPULATIONS",
                            "doi": "10.1093/genetics/16.2.97",
                            "journal": "Genetics",
                            "date": { "year": 1931 }
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-1/entries"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .add_to_collection(Parameters(AddToCollectionArgs {
                collection: "My References".into(),
                query: "Wright 1931 genetics shifting balance".into(),
                use_live_queries: false,
            }))
            .await;

        assert!(result.contains("Added to My References"));
        assert!(!result.contains("Ambiguous match"));
    }

    #[tokio::test]
    async fn test_add_to_collection_surfaces_ambiguous_candidates() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-1", "name": "My References", "entry_count": 0}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": [
                    {
                        "metadata": {
                            "title": "Shifting Balance in Evolution",
                            "doi": "10.1093/genetics/16.2.97",
                            "journal": "Genetics",
                            "date": { "year": 1931 }
                        }
                    },
                    {
                        "metadata": {
                            "title": "Another Wright Paper",
                            "doi": "10.1093/genetics/16.2.98",
                            "journal": "Genetics",
                            "date": { "year": 1932 }
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-1/entries"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .add_to_collection(Parameters(AddToCollectionArgs {
                collection: "My References".into(),
                query: "Wright 1931 genetics shifting balance".into(),
                use_live_queries: false,
            }))
            .await;

        assert!(result.contains("Ambiguous match"));
        assert!(result.contains("Shifting Balance in Evolution"));
        assert!(result.contains("10.1093/genetics/16.2.97"));
    }

    #[tokio::test]
    async fn test_batch_add_to_collection_prefers_resolve_for_free_text() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "col-1",
                "name": "RuhiMastersThesis"
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "paper": {
                    "title": "Shifting Balance in Evolution",
                    "doi": "10.1093/genetics/16.2.97"
                }
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            // The ranker is consulted on every free-text resolve now, so the
            // resolver's answer can be checked against a ranked candidate
            // set instead of being taken on its own word. An empty ranking
            // cannot overrule it, so the outcome below is unchanged.
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-1/entries/batch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .batch_add_to_collection(Parameters(BatchAddArgs {
                collection: "RuhiMastersThesis".into(),
                queries: vec!["Wright 1931 genetics shifting balance".into()],
                use_live_queries: false,
            }))
            .await;

        assert!(result.contains("Added 1 to 'RuhiMastersThesis'"));
        assert!(!result.contains("Could not resolve"));
    }

    /// `batch_add_to_collection` auto-creates a missing target collection and
    /// accepts a mixed DOI plus free-text query batch.
    #[tokio::test]
    async fn test_batch_add_to_collection_auto_creates_for_mixed_queries() {
        let mock = MockServer::start().await;

        // Collection does not exist yet.
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .expect(1)
            .mount(&mock)
            .await;

        // Create succeeds and returns the new id.
        Mock::given(method("POST"))
            .and(path("/api/v1/collections"))
            .and(body_string_contains("amsel-literature-survey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "col-amsel",
                "name": "amsel-literature-survey"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // DOI lookup path for the one DOI query.
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "Doi Paper",
                "doi": "10.1000/demo"
            })))
            .mount(&mock)
            .await;

        // Free-text resolve path returns a verified paper for everything except
        // the last query, which we leave to fall through so we also cover the
        // partial-success diagnostics branch.
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains("Wright"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "paper": {
                    "title": "Shifting Balance in Evolution",
                    "doi": "10.1093/genetics/16.2.97"
                }
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains("Fisher"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "paper": {
                    "title": "The Genetical Theory of Natural Selection",
                    "doi": "10.5962/bhl.title.27468"
                }
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains("Haldane"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "paper": {
                    "title": "The Causes of Evolution",
                    "doi": "10.1515/9781400882588"
                }
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains("Kimura"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "paper": {
                    "title": "The Neutral Theory of Molecular Evolution",
                    "doi": "10.1017/CBO9780511623486"
                }
            })))
            .mount(&mock)
            .await;
        // Unresolvable free-text query: resolve returns no candidates and the
        // reverse fallback returns nothing either.
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains("qwertyzzz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "candidates": []
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&mock)
            .await;

        // Batch add endpoint should be hit with all resolved entries.
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-amsel/entries/batch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 5,
                "duplicates_skipped": 0
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .batch_add_to_collection(Parameters(BatchAddArgs {
                collection: "amsel-literature-survey".into(),
                queries: vec![
                    "10.1000/demo".into(),
                    "Wright 1931 genetics shifting balance".into(),
                    "Fisher 1930 genetical theory".into(),
                    "Haldane 1932 causes of evolution".into(),
                    "Kimura 1983 neutral theory".into(),
                    "qwertyzzz nothing matches this".into(),
                ],
                use_live_queries: false,
            }))
            .await;

        assert!(
            result.contains("Added 5 to 'amsel-literature-survey'"),
            "expected auto-create + 5 added, got: {result}"
        );
        assert!(
            !result.contains("Failed to create collection"),
            "must not surface generic create failure, got: {result}"
        );
        assert!(
            result.contains("Unresolved:"),
            "expected partial-success diagnostics, got: {result}"
        );
    }

    /// Regression guard: `resolve_or_create_collection` must not swallow a
    /// 5xx on the LIST step with a misleading "Failed to create collection"
    /// after blindly retrying CREATE. It must surface the original list error
    /// so operators can see what actually broke.
    #[tokio::test]
    async fn test_resolve_or_create_collection_propagates_list_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "message": "collections backend unavailable"
            })))
            .mount(&mock)
            .await;
        // No CREATE mock on purpose: if the code incorrectly falls through to
        // CREATE the call will 404 from wiremock and the assertion below will
        // still catch the wrong error prefix.

        let s = test_server(&mock.uri());
        let err = s
            .resolve_or_create_collection("amsel-literature-survey")
            .await
            .expect_err("should propagate list failure");
        assert!(
            err.contains("Failed to list collections"),
            "expected list failure to propagate, got: {err}"
        );
        assert!(
            !err.contains("Failed to create collection"),
            "must not mask list failure with create error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_expand_journal_success() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/journal/expand"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "abbreviation": "JACS",
                "full_name": "Journal of the American Chemical Society",
                "found": true
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .expand_journal(Parameters(ExpandJournalArgs {
                abbreviation: "JACS".into(),
            }))
            .await;
        assert!(result.contains("Journal of the American Chemical Society"));
    }

    #[tokio::test]
    async fn test_expand_journal_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/journal/expand"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "abbreviation": "XYZ",
                "full_name": null,
                "found": false
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .expand_journal(Parameters(ExpandJournalArgs {
                abbreviation: "XYZ".into(),
            }))
            .await;
        assert!(result.contains("No expansion found"));
    }

    #[tokio::test]
    async fn test_error_detail_surfaces_plan_gating() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-123/entries"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": "plan_required",
                "message": "This feature requires an Academic (EUR 4/mo) or Business (EUR 10/mo) plan."
            })))
            .mount(&mock)
            .await;

        // Manually call the endpoint to test error_detail
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/api/v1/collections/col-123/entries", mock.uri()))
            .json(&serde_json::json!({"metadata": {}}))
            .send()
            .await
            .unwrap();
        let detail = error_detail(resp).await;
        assert!(detail.contains("Academic"));
        assert!(detail.contains("EUR 4/mo"));
    }

    #[tokio::test]
    async fn batch_format_forwards_the_caller_locale_to_the_format_api() {
        let mock = MockServer::start().await;
        let doi = "10.5555/batch.locale.1";
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "title": "Un article à formater en français",
                "authors": [{"family": "Dupont", "given": "A."}],
                "date": {"year": 2026}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        // The single /api/v1/format call must carry `"locale":"fr-FR"`,
        // the value the caller passed to batch_format. The old path
        // hard-coded en-US and ignored the arg.
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .and(body_string_contains(r#""locale":"fr-FR""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{"intext_plain": "(Dupont, 2026)", "bib_plain": "Dupont. Article."}]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let out = test_server(&mock.uri())
            .batch_format(Parameters(crate::tool_args::BatchArgs {
                citations: vec![doi.into()],
                style: "apa".into(),
                locale: "fr-FR".into(),
                use_live_queries: false,
            }))
            .await;

        assert!(out.contains("Dupont"), "{out}");
    }

    #[tokio::test]
    async fn format_citation_renders_bibtex_from_the_record_without_calling_the_format_api() {
        let mock = MockServer::start().await;
        let doi = "10.5555/format.bibtex.1";
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "title": "A record rendered as BibTeX",
                "entry_type": "journal-article",
                "authors": [{"family": "Example", "given": "A."}],
                "date": {"year": 2026},
                "journal": "Journal of Synthetic Fixtures",
                "volume": "12",
                "issue": "3",
                "pages": "101-115",
                "publisher": "Example Press"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        // /api/v1/format is NOT called for bibtex: the entry is rendered
        // in-process from the merged record.
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .format_citation(Parameters(FormatArgs {
                doi: doi.into(),
                style: "apa".into(),
                locale: "en-US".into(),
                format: "bibtex".into(),
            }))
            .await;

        assert!(text.contains("@article{"), "{text}");
        assert!(text.contains("A record rendered as"), "{text}");
        assert!(text.contains("Example"), "{text}");
        assert!(text.contains("doi = {10.5555/format.bibtex.1"), "{text}");
        assert!(text.contains("volume = {12}"), "{text}");
        assert!(text.contains("pages = {101--115}"), "{text}");
    }

    #[tokio::test]
    async fn format_citation_bibtex_always_writes_the_requested_doi_even_if_the_record_omits_it() {
        let mock = MockServer::start().await;
        let doi = "10.5555/bibtex.doi.seed.1";
        // The OokCite body returns a record with every field except
        // `doi`, which the registrar sometimes omits when the stored
        // row was imported from a .bib without one. The renderer used
        // to emit `@article` without `doi = {...}`, so an agent could
        // not round-trip the entry back to a DOI.
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "A record that does not store the DOI itself",
                "authors": [{"family": "Example", "given": "A."}],
                "date": {"year": 2026}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .format_citation(Parameters(FormatArgs {
                doi: doi.into(),
                style: "apa".into(),
                locale: "en-US".into(),
                format: "bibtex".into(),
            }))
            .await;

        assert!(text.contains("@article{"), "{text}");
        assert!(
            text.contains(&format!("doi = {{{doi}")),
            "requested DOI was dropped from the BibTeX entry: {text}"
        );
    }

    #[tokio::test]
    async fn format_citation_renders_ris_from_the_record() {
        let mock = MockServer::start().await;
        let doi = "10.5555/format.ris.1";
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "title": "A record exported as RIS",
                "entry_type": "journal-article",
                "authors": [
                    {"family": "Example", "given": "A."},
                    {"family": "Second", "given": "B."}
                ],
                "date": {"year": 2026},
                "journal": "Journal of Synthetic Fixtures",
                "volume": "5",
                "issue": "2",
                "pages": "7-21",
                "publisher": "Example Press"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .format_citation(Parameters(FormatArgs {
                doi: doi.into(),
                style: "apa".into(),
                locale: "en-US".into(),
                format: "ris".into(),
            }))
            .await;

        assert!(text.starts_with("TY  - JOUR"), "{text}");
        assert!(text.contains("TI  - A record exported as RIS"), "{text}");
        assert!(text.contains("AU  - Example, A."), "{text}");
        assert!(text.contains("AU  - Second, B."), "{text}");
        assert!(text.contains("PY  - 2026"), "{text}");
        assert!(
            text.contains("JO  - Journal of Synthetic Fixtures"),
            "{text}"
        );
        assert!(text.contains("VL  - 5"), "{text}");
        assert!(text.contains("IS  - 2"), "{text}");
        assert!(text.contains("SP  - 7"), "{text}");
        assert!(text.contains("EP  - 21"), "{text}");
        assert!(text.contains("DO  - 10.5555/format.ris.1"), "{text}");
        assert!(text.trim_end().ends_with("ER  -"), "{text}");
    }

    #[tokio::test]
    async fn format_citation_forwards_the_caller_locale_to_the_format_api() {
        let mock = MockServer::start().await;
        let doi = "10.5555/format.locale.1";
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": doi,
                "title": "A paper in French format",
                "authors": [{"family": "Dupont", "given": "A."}],
                "date": {"year": 2026}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/format"))
            .and(body_string_contains(r#""locale":"fr-FR""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plain": "Dupont, A. (2026). Un article.",
                "citations": [{"plain": "(Dupont, 2026)"}]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .format_citation(Parameters(FormatArgs {
                doi: doi.into(),
                style: "apa".into(),
                locale: "fr-FR".into(),
                format: "csl".into(),
            }))
            .await;

        assert!(text.contains("Dupont"), "{text}");
        assert!(text.contains("In-text: (Dupont, 2026)"), "{text}");
    }

    #[tokio::test]
    async fn format_citation_refuses_an_unknown_format_before_a_request() {
        let mock = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let text = test_server(&mock.uri())
            .format_citation(Parameters(FormatArgs {
                doi: "10.5555/format.unknown.1".into(),
                style: "apa".into(),
                locale: "en-US".into(),
                format: "jsonld".into(),
            }))
            .await;

        assert!(text.starts_with("CLIENT ERROR:"), "{text}");
        assert!(text.contains("\"jsonld\""), "{text}");
    }

    #[tokio::test]
    async fn test_format_citation_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(429).set_body_string(format!(
                "Daily limit reached ({}/day). Resets in 5h.",
                crate::constants::FREE_DAILY_LOOKUPS
            )))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .format_citation(Parameters(FormatArgs {
                doi: "10.1038/187493a0".into(),
                style: "apa".into(),
                locale: "en-US".into(),
                format: "csl".into(),
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED"));
        assert!(!result.contains("not found"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Daily limit reached"))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "test".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_temporary_upstream_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("Retry-After", "0")
                    .set_body_string(
                        "Lookup service temporarily unavailable. Please try again shortly.",
                    ),
            )
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "test".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;
        assert!(result.starts_with("TEMPORARY ERROR"));
        assert!(!result.contains("No matches"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_503_retries_reverse_once_without_resolve() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("Retry-After", "0")
                    .set_body_string("SearchPressureError"),
            )
            .expect(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "candidates": []
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "test".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;
        assert!(result.starts_with("TEMPORARY ERROR"));
        assert!(!result.contains("No matches"));
    }

    #[tokio::test]
    async fn test_isbn_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/isbn"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Daily limit reached"))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .lookup_isbn(Parameters(IsbnArgs {
                isbn: "978-0-521-85629-7".into(),
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED"));
    }

    #[tokio::test]
    async fn test_no_crossref_leak_in_errors() {
        let mock = MockServer::start().await;
        // 404 should not mention CrossRef
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.9999/fake".into(),
                ..Default::default()
            }))
            .await;
        assert!(
            !result.to_lowercase().contains("crossref"),
            "Error leaked 'CrossRef': {result}"
        );
        assert!(
            !result.to_lowercase().contains("openlibrary"),
            "Error leaked 'OpenLibrary': {result}"
        );
    }

    // --- usage / batch_resolve / entry-edit helpers ---

    #[test]
    fn usage_report_shows_daily_and_monthly_allowances() {
        let out = format_usage_report(&serde_json::json!({
            "plan": "academic",
            "authenticated": true,
            "daily": { "limit": 20000, "remaining": 19993 },
            "monthly": {
                "limit": 10000,
                "used": 12,
                "remaining": 9988,
                "allows_overage": false
            }
        }));
        assert!(out.contains("Plan: academic"), "{out}");
        assert!(
            out.contains("Daily lookups: 19993 remaining of 20000"),
            "{out}"
        );
        assert!(
            out.contains("Monthly lookups: 9988 remaining of 10000 (12 used)"),
            "{out}"
        );
        assert!(out.contains("Monthly overage: not allowed"), "{out}");
    }

    #[test]
    fn usage_report_omits_monthly_block_when_plan_has_none() {
        let out = format_usage_report(&serde_json::json!({
            "plan": "free",
            "authenticated": false,
            "daily": { "limit": 20, "remaining": 3 },
            "monthly": serde_json::Value::Null
        }));
        assert!(out.contains("Daily lookups: 3 remaining of 20"), "{out}");
        assert!(!out.contains("Monthly"), "{out}");
        assert!(out.contains("anonymous IP limits apply"), "{out}");
    }

    #[test]
    fn person_from_name_reads_both_author_orders() {
        assert_eq!(
            person_from_name("Goswami, Rohit"),
            serde_json::json!({"family": "Goswami", "given": "Rohit"})
        );
        assert_eq!(
            person_from_name("Rohit Goswami"),
            serde_json::json!({"family": "Goswami", "given": "Rohit"})
        );
        assert_eq!(
            person_from_name("  Jonsson  "),
            serde_json::json!({"family": "Jonsson"})
        );
        assert_eq!(
            person_from_name("Goswami,"),
            serde_json::json!({"family": "Goswami"})
        );
        assert_eq!(
            person_from_name("{IEEE Computer Society}"),
            serde_json::json!({"family": "IEEE Computer Society", "literal": true})
        );
        assert_eq!(
            person_from_name("{IEEE Computer Society},"),
            serde_json::json!({"family": "IEEE Computer Society", "literal": true})
        );
        assert_eq!(
            person_from_name("David van der Spoel"),
            serde_json::json!({"family": "Spoel", "given": "David", "prefix": "van der"})
        );
        assert_eq!(
            person_from_name("De Souza, Roger A."),
            serde_json::json!({"family": "De Souza", "given": "Roger A."})
        );
    }

    #[test]
    fn entry_metadata_overrides_set_publication_fields_and_can_delete() {
        let current = serde_json::json!({
            "id": "kept",
            "entry_type": "techreport",
            "title": "A paper",
            "authors": [{"family": "Goswami"}],
            "journal": "ACM Press",
            "affiliation": "University",
            "editor": [{"family": "Someone"}],
            "issn": "0000-0000",
            "date": {"year": 1990, "month": 6}
        });
        let args = UpdateEntryMetadataArgs {
            collection: "Refs".into(),
            entry_id: "e1".into(),
            booktitle: Some("Proceedings of PLDI".into()),
            entry_type: Some("inproceedings".into()),
            institution: Some("IETF".into()),
            chapter: Some("1".into()),
            pages: Some("92-101".into()),
            version: Some("3.6.0".into()),
            eid: Some("134106".into()),
            number: Some("8259".into()),
            citation_key: Some("clingerHowReadFloating1990".into()),
            authors: Some(vec!["{IEEE Computer Society}".into()]),
            journal: Some(String::new()),
            clear: Some(vec!["affiliation".into(), "editor".into(), "issn".into()]),
            ..Default::default()
        };
        let merged = apply_entry_metadata_overrides(&current, &args);
        assert_eq!(merged["id"], "kept");
        assert_eq!(merged["booktitle"], "Proceedings of PLDI");
        assert_eq!(merged["entry_type"], "inproceedings");
        assert_eq!(merged["institution"], "IETF");
        assert_eq!(merged["chapter"], "1");
        assert_eq!(merged["pages"], "92-101");
        assert_eq!(merged["version"], "3.6.0");
        assert_eq!(merged["eid"], "134106");
        assert_eq!(merged["number"], "8259");
        assert_eq!(merged["citation_key"], "clingerHowReadFloating1990");
        assert!(merged.get("journal").is_none(), "{merged}");
        assert!(merged.get("affiliation").is_none(), "{merged}");
        assert!(merged.get("editor").is_none(), "{merged}");
        assert!(merged.get("issn").is_none(), "{merged}");
        assert_eq!(merged["date"]["month"], 6);
        assert_eq!(
            merged["authors"][0],
            serde_json::json!({"family": "IEEE Computer Society", "literal": true})
        );
        let locked = merged["user_fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>();
        assert!(locked.contains(&"booktitle"), "{locked:?}");
        assert!(locked.contains(&"authors"), "{locked:?}");
        assert!(!locked.contains(&"journal"), "{locked:?}");
        assert!(!locked.contains(&"affiliation"), "{locked:?}");

        let cleared = apply_entry_metadata_overrides(
            &merged,
            &UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                clear: Some(vec!["year".into(), "authors".into()]),
                ..Default::default()
            },
        );
        assert!(cleared["date"].get("year").is_none(), "{cleared}");
        assert_eq!(cleared["date"]["month"], 6);
        assert!(cleared.get("authors").is_none(), "{cleared}");
    }

    #[test]
    fn entry_metadata_overrides_keep_untouched_fields() {
        let current = serde_json::json!({
            "id": "goswami2026",
            "entry_type": "Article",
            "title": "Wrong Title",
            "authors": [{"family": "Goswami", "given": "Rohit"}],
            "journal": "MethodsX",
            "date": {"year": 2020, "month": 4},
            "issue": "3"
        });
        let args = UpdateEntryMetadataArgs {
            collection: "Refs".into(),
            entry_id: "e1".into(),
            title: Some("Right Title".into()),
            year: Some(2026),
            ..Default::default()
        };
        let merged = apply_entry_metadata_overrides(&current, &args);
        assert_eq!(merged["title"], serde_json::json!("Right Title"));
        assert_eq!(merged["date"]["year"], serde_json::json!(2026));
        // Required fields the caller never mentioned survive the replace.
        assert_eq!(merged["id"], serde_json::json!("goswami2026"));
        assert_eq!(merged["entry_type"], serde_json::json!("Article"));
        assert_eq!(merged["journal"], serde_json::json!("MethodsX"));
        assert_eq!(merged["issue"], serde_json::json!("3"));
        // Month is part of the same DateValue and must not be dropped by a year edit.
        assert_eq!(merged["date"]["month"], serde_json::json!(4));
        assert_eq!(merged["authors"][0]["family"], serde_json::json!("Goswami"));
    }

    #[test]
    fn batch_resolve_body_tags_every_input_as_text() {
        let body = batch_resolve_request_body(&["Einstein 1905".into(), "10.1/x".into()], true);
        let inputs = body["inputs"].as_array().expect("inputs array");
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["input"]["kind"], serde_json::json!("text"));
        assert_eq!(
            inputs[0]["input"]["text"],
            serde_json::json!("Einstein 1905")
        );
        assert_eq!(
            inputs[1]["options"]["use_live_queries"],
            serde_json::json!(true)
        );
        assert!(
            body.get("async").is_none(),
            "sync batch must not request a job: {body}"
        );
    }

    #[test]
    fn batch_resolve_results_follow_input_order() {
        let citations = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];
        // Server returns them out of order, tagged by index; one errored and one
        // never came back at all.
        let payload = serde_json::json!({
            "results": [
                {
                    "index": 2,
                    "status": "error",
                    "message": "no candidates"
                },
                {
                    "index": 0,
                    "status": "ok",
                    "paper": {
                        "title": "Universal Nucleation Behavior of Sheared Systems",
                        "doi": "10.1103/physrevlett.126.195702",
                        "journal": "PRL",
                        "date": {"year": 2021},
                        "authors": [{"family": "Goswami"}]
                    }
                }
            ]
        });
        let lines = format_batch_resolve_results(&citations, &payload);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("1. RESOLVED"), "{}", lines[0]);
        assert!(lines[0].contains("doi:10.1103/physrevlett.126.195702"));
        assert!(
            lines[1].starts_with("2. NO RESULT | second"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].starts_with("3. ERROR | third | no candidates"),
            "{}",
            lines[2]
        );
    }

    #[test]
    fn batch_resolve_marks_duplicate_response_indexes_as_aligned_errors() {
        let citations = vec!["first".to_string(), "second".to_string()];
        let payload = serde_json::json!({
            "results": [
                {
                    "index": 0,
                    "status": "ok",
                    "paper": {"title": "First response", "doi": "10.1/first"}
                },
                {
                    "index": 0,
                    "status": "error",
                    "message": "conflicting response"
                },
                {
                    "index": 1,
                    "status": "ok",
                    "paper": {"title": "Second response", "doi": "10.1/second"}
                }
            ]
        });

        let lines = format_batch_resolve_results(&citations, &payload);
        assert_eq!(
            lines[0],
            "1. ERROR | first | response contained duplicate result index"
        );
        assert!(
            lines[1].starts_with("2. RESOLVED | Second response"),
            "{}",
            lines[1]
        );
        assert!(!lines[0].contains("First response"));
        assert!(!lines[0].contains("conflicting response"));
    }

    #[tokio::test]
    async fn batch_resolve_refuses_over_the_sync_limit_without_calling_the_api() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve/batch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": []
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let citations: Vec<String> = (0..SYNC_BATCH_RESOLVE_LIMIT + 1)
            .map(|i| format!("citation {i}"))
            .collect();
        let out = test_server(&mock.uri())
            .batch_resolve(Parameters(BatchResolveArgs {
                citations,
                use_live_queries: false,
            }))
            .await;
        assert!(out.starts_with("REFUSED"), "{out}");
        assert!(out.contains("50-input limit"), "{out}");
    }

    #[tokio::test]
    async fn usage_reports_the_plan_from_me_usage() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "business",
                "authenticated": true,
                "daily": {"limit": 20000, "remaining": 42}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let out = test_server(&mock.uri())
            .usage(Parameters(UsageArgs {}))
            .await;
        assert!(out.contains("Plan: business"), "{out}");
        assert!(out.contains("42 remaining of 20000"), "{out}");
    }

    #[tokio::test]
    async fn update_entry_metadata_sends_the_merged_record() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "c1", "name": "Refs"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/c1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c1",
                "name": "Refs",
                "entries": [{
                    "id": "e1",
                    "metadata": {
                        "id": "goswami2026",
                        "entry_type": "Article",
                        "title": "Wrong Title",
                        "authors": [{"family": "Goswami"}],
                        "journal": "MethodsX"
                    }
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/collections/c1/entries/e1/metadata"))
            .and(body_string_contains(r#""title":"Right Title""#))
            .and(body_string_contains(r#""entry_type":"Article""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "e1",
                "added_at": "2026-08-14T00:00:00Z",
                "metadata": {"title": "Right Title"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let out = test_server(&mock.uri())
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                title: Some("Right Title".into()),
                ..Default::default()
            }))
            .await;
        assert!(out.contains("Updated entry e1"), "{out}");
        assert!(out.contains("Right Title"), "{out}");
    }

    #[tokio::test]
    async fn update_entry_metadata_records_a_local_override() {
        let dir = tempfile::tempdir().unwrap();
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "c1", "name": "Refs"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/c1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c1",
                "name": "Refs",
                "entries": [{
                    "id": "e1",
                    "metadata": {
                        "entry_type": "Article",
                        "title": "P<scp>ACKMOL</scp>",
                        "doi": "10.1002/JCC.21224",
                        "authors": [{"family": "Martinez"}]
                    }
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/collections/c1/entries/e1/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "e1",
                "metadata": {"title": "P<scp>ACKMOL</scp>"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let server = test_server(&mock.uri())
            .with_overrides(overrides::OverrideStore::Enabled(dir.path().to_path_buf()));
        let out = server
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                citation_key: Some("martinezPackmol2009".into()),
                clear: Some(vec!["url".into()]),
                ..Default::default()
            }))
            .await;
        assert!(out.contains("Updated entry e1"), "{out}");
        assert!(
            out.contains("Saved a local override for 10.1002/JCC.21224 in collection 'Refs'."),
            "{out}"
        );
        let shown = dir.path().to_string_lossy().into_owned();
        assert!(!out.contains(&shown) && !out.contains(".json"), "{out}");

        let out = server
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "doi:10.1002/jcc.21224".into(),
                title: Some("{PACKMOL}: A package".into()),
                local_only: true,
                ..Default::default()
            }))
            .await;
        assert!(out.contains("The server record was not changed"), "{out}");
        assert!(!out.contains(&shown) && !out.contains(".json"), "{out}");

        let store = overrides::load_from(dir.path(), "Refs").unwrap();
        let entry = &store.entries["doi:10.1002/jcc.21224"];
        assert_eq!(
            entry.fields["citation_key"].as_deref(),
            Some("martinezPackmol2009")
        );
        assert_eq!(
            entry.fields["title"].as_deref(),
            Some("{PACKMOL}: A package")
        );
        assert_eq!(entry.fields["url"], None);
    }

    /// GET mocks for collection "Refs" (id c1) with one entry carrying a year.
    async fn mount_refs_collection(mock: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "c1", "name": "Refs"}
            ])))
            .mount(mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/c1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c1",
                "name": "Refs",
                "entries": [{
                    "id": "e1",
                    "metadata": {
                        "entry_type": "Article",
                        "title": "Packmol",
                        "doi": "10.1002/jcc.21224",
                        "journal": "Journal of Computational Chemistry",
                        "date": {"year": 2009},
                        "authors": [{"family": "Martinez", "given": "L."}]
                    }
                }]
            })))
            .mount(mock)
            .await;
    }

    fn export_args() -> ExportCollectionArgs {
        ExportCollectionArgs {
            collection: "Refs".into(),
            format: "bib".into(),
            style: "apa".into(),
            locale: "en-US".into(),
            software_entry: "software".into(),
            eid_as: "eid".into(),
            local_overrides: true,
            enrich: false,
        }
    }

    #[tokio::test]
    async fn shared_http_server_keeps_no_local_overrides() {
        let dir = tempfile::tempdir().unwrap();
        // A file that would change the export if it were read.
        let mut planted = overrides::CollectionOverrides::new("Refs");
        planted.record(
            "doi:10.1002/jcc.21224".into(),
            &UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                citation_key: Some("leakedKey".into()),
                ..Default::default()
            },
        );
        overrides::save_to(dir.path(), "Refs", &planted).unwrap();
        let before = std::fs::read_to_string(dir.path().join("Refs.json")).unwrap();
        let _dir = EnvGuard::set("OOKCITE_OVERRIDES_DIR", dir.path().to_str().unwrap());

        let mock = MockServer::start().await;
        mount_refs_collection(&mock).await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/collections/c1/entries/e1/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "e1",
                "metadata": {"title": "Packmol"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        for auth in [
            crate::inbound_auth::HttpAuthMode::Oauth,
            crate::inbound_auth::HttpAuthMode::Bearer,
        ] {
            let store = overrides::OverrideStore::for_transport(overrides::Transport::Http(auth));
            assert_eq!(store, overrides::OverrideStore::Disabled, "{auth:?}");
        }
        let server =
            test_server(&mock.uri()).with_overrides(overrides::OverrideStore::for_transport(
                overrides::Transport::Http(crate::inbound_auth::HttpAuthMode::Oauth),
            ));

        let out = server
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                citation_key: Some("otherUserKey".into()),
                local_only: true,
                ..Default::default()
            }))
            .await;
        assert!(out.contains("not kept by this shared server"), "{out}");
        assert!(out.contains("The server record was not changed"), "{out}");

        let out = server
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                citation_key: Some("otherUserKey".into()),
                ..Default::default()
            }))
            .await;
        assert!(out.contains("Updated entry e1"), "{out}");
        assert!(!out.contains("local override"), "{out}");

        let bib = server.export_collection(Parameters(export_args())).await;
        assert!(bib.contains("@article{"), "{bib}");
        assert!(!bib.contains("leakedKey"), "{bib}");
        let after = std::fs::read_to_string(dir.path().join("Refs.json")).unwrap();
        assert_eq!(before, after);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn export_skips_a_corrupt_override_file_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Refs.json"), "{\"entries\": [oops").unwrap();
        let mock = MockServer::start().await;
        mount_refs_collection(&mock).await;
        let server = test_server(&mock.uri())
            .with_overrides(overrides::OverrideStore::Enabled(dir.path().to_path_buf()));
        let bib = server.export_collection(Parameters(export_args())).await;
        assert!(
            bib.starts_with("% Warning: the local override file for collection 'Refs'"),
            "{bib}"
        );
        assert!(bib.contains("@article{"), "{bib}");
        assert!(bib.contains("year = {2009}"), "{bib}");
        let shown = dir.path().to_string_lossy().into_owned();
        assert!(!bib.contains(&shown) && !bib.contains("Refs.json"), "{bib}");
    }

    #[tokio::test]
    async fn local_clear_removes_the_year_from_the_export() {
        let dir = tempfile::tempdir().unwrap();
        let mock = MockServer::start().await;
        mount_refs_collection(&mock).await;
        let server = test_server(&mock.uri())
            .with_overrides(overrides::OverrideStore::Enabled(dir.path().to_path_buf()));
        let out = server
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                clear: Some(vec!["year".into(), "journal".into()]),
                local_only: true,
                ..Default::default()
            }))
            .await;
        assert!(out.contains("Saved a local override"), "{out}");
        let bib = server.export_collection(Parameters(export_args())).await;
        assert!(bib.contains("@article{"), "{bib}");
        assert!(!bib.contains("year ="), "{bib}");
        assert!(!bib.contains("journal ="), "{bib}");
    }

    #[tokio::test]
    async fn update_entry_metadata_refuses_an_empty_edit() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(0)
            .mount(&mock)
            .await;

        let out = test_server(&mock.uri())
            .update_entry_metadata(Parameters(UpdateEntryMetadataArgs {
                collection: "Refs".into(),
                entry_id: "e1".into(),
                ..Default::default()
            }))
            .await;
        assert!(out.starts_with("Nothing to update."), "{out}");
    }

    #[tokio::test]
    async fn merge_entries_refuses_when_both_references_are_the_same_entry() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "c1", "name": "Refs"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/c1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c1",
                "entries": [{
                    "id": "e1",
                    "metadata": {"title": "A paper", "doi": "10.1/x"}
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/c1/entries/e1/merge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;

        // Same entry reached by its opaque id and by its DOI alias.
        let out = test_server(&mock.uri())
            .merge_entries(Parameters(MergeEntriesArgs {
                collection: "Refs".into(),
                keep_entry_id: "e1".into(),
                absorb_entry_id: "10.1/x".into(),
            }))
            .await;
        assert!(out.contains("nothing to merge"), "{out}");
    }

    #[tokio::test]
    async fn orcid_search_needs_at_least_one_filter() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/orcid/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits": []})))
            .expect(0)
            .mount(&mock)
            .await;

        let out = test_server(&mock.uri())
            .orcid_search(Parameters(OrcidSearchArgs {
                author: Some("   ".into()),
                affiliation: None,
                orcid: None,
                limit: None,
            }))
            .await;
        assert!(out.contains("Provide at least one of"), "{out}");
    }

    #[tokio::test]
    async fn orcid_profile_reports_a_missing_id_plainly() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/orcid/0000-0002-2393-8056"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let out = test_server(&mock.uri())
            .orcid_profile(Parameters(OrcidProfileArgs {
                orcid_id: "0000-0002-2393-8056".into(),
            }))
            .await;
        assert_eq!(out, "No ORCID profile found for 0000-0002-2393-8056.");
    }

    fn structured_keys(result: &CallToolResult) -> usize {
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.as_object())
            .map(|object| object.len())
            .unwrap_or(0)
    }

    #[test]
    fn promote_moves_a_structured_marker_into_structured_content_and_strips_the_text() {
        let prose = "VALID\nDOI: 10.5555/record\nTitle: X";
        let record = serde_json::json!({
            "kind": crate::work_record::WORK_RECORD_KIND,
            "doi": "10.5555/record",
            "source": "ookcite_corpus"
        });
        let wrapped = crate::work_record::append_structured_marker(prose, &record);
        let mut result = CallToolResult::success(vec![ContentBlock::text(wrapped)]);

        promote_structured_marker(&mut result);

        let text = tool_result_text(&result);
        assert!(!text.contains("OOKCITE_STRUCTURED_JSON"), "{text}");
        assert!(text.starts_with("VALID"), "{text}");
        assert!(text.contains("Title: X"), "{text}");
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["kind"], crate::work_record::WORK_RECORD_KIND);
        assert_eq!(structured["doi"], "10.5555/record");
    }

    #[test]
    fn promote_leaves_an_upstream_failure_payload_in_place_when_the_text_has_no_marker() {
        let mut result = CallToolResult::success(vec![ContentBlock::text(
            "RATE LIMITED: 429 Too Many Requests: Daily limit reached (Retry-After: 30)",
        )]);
        promote_upstream_failure(&mut result);
        promote_structured_marker(&mut result);
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["kind"], "rate_limited");
    }

    #[test]
    fn promote_marks_a_rate_limit_and_keeps_the_delay() {
        let text = "RATE LIMITED 10.2/cap : 429 Too Many Requests: Daily limit (Retry-After: 75)\nCheck remaining quota";
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured_keys(&result), 4);
        assert_eq!(structured["kind"], "rate_limited");
        assert_eq!(structured["http_status"], 429);
        assert_eq!(structured["retry_after"], "75");
        assert_eq!(structured["message"], text);
    }

    #[test]
    fn promote_marks_the_unavailable_ridley_inbox_as_an_error_without_a_prefix() {
        let mut result = CallToolResult::success(vec![ContentBlock::text(
            crate::ridley_library::SAVE_INBOX_UNAVAILABLE,
        )]);
        promote_upstream_failure(&mut result);

        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["kind"], "ridley_save_inbox_unavailable");
        assert_eq!(structured["http_status"], 404);
        assert_eq!(
            structured["message"],
            crate::ridley_library::SAVE_INBOX_UNAVAILABLE
        );
    }

    #[test]
    fn promote_keeps_an_http_date_and_a_malformed_delay() {
        let date = "TIMEOUT: 504 Gateway Timeout (Retry-After: Wed, 21 Oct 2015 07:28:00 GMT)";
        let mut dated = CallToolResult::success(vec![ContentBlock::text(date)]);
        promote_upstream_failure(&mut dated);
        let structured = dated.structured_content.as_ref().unwrap();
        assert_eq!(structured["kind"], "timeout");
        assert_eq!(structured["http_status"], 504);
        assert_eq!(structured["retry_after"], "Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(structured_keys(&dated), 4);

        let mut malformed = CallToolResult::success(vec![ContentBlock::text(
            "TEMPORARY ERROR: 503 Service Unavailable (Retry-After: not-a-delay)",
        )]);
        promote_upstream_failure(&mut malformed);
        let structured = malformed.structured_content.as_ref().unwrap();
        assert_eq!(structured["kind"], "temporary_error");
        assert_eq!(structured["http_status"], 503);
        assert_eq!(structured["retry_after"], "not-a-delay");
        assert_eq!(structured_keys(&malformed), 4);
    }

    #[test]
    fn promote_keeps_a_request_timeout_status() {
        let text =
            "TIMEOUT 10.1038/request-timeout : 408 Request Timeout: request expired (Retry-After: 4)";
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        promote_upstream_failure(&mut result);

        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured_keys(&result), 4);
        assert_eq!(structured["kind"], "timeout");
        assert_eq!(structured["http_status"], 408);
        assert_eq!(structured["retry_after"], "4");
        assert_eq!(structured["message"], text);
    }

    #[test]
    fn promote_leaves_a_miss_and_records_an_absent_delay_as_null() {
        let mut miss = CallToolResult::success(vec![ContentBlock::text("Not found")]);
        promote_upstream_failure(&mut miss);
        assert_eq!(miss.is_error, Some(false));
        assert!(miss.structured_content.is_none());

        let mut absent = CallToolResult::success(vec![ContentBlock::text(
            "RATE LIMITED: 429 Too Many Requests: Daily limit reached",
        )]);
        promote_upstream_failure(&mut absent);
        let structured = absent.structured_content.as_ref().unwrap();
        assert!(structured["retry_after"].is_null());
        assert_eq!(structured_keys(&absent), 4);

        let mut transport = CallToolResult::success(vec![ContentBlock::text(
            "ERROR 10.1/x : error sending request",
        )]);
        promote_upstream_failure(&mut transport);
        assert_eq!(transport.is_error, Some(true));
        let structured = transport.structured_content.as_ref().unwrap();
        assert_eq!(structured_keys(&transport), 4);
        assert_eq!(structured["kind"], "error");
        assert!(structured["http_status"].is_null());

        let mut unregistered = CallToolResult::success(vec![ContentBlock::text(
            "UNREGISTERED 10.2307/4615733: the DOI is not registered at doi.org",
        )]);
        promote_upstream_failure(&mut unregistered);
        assert_eq!(unregistered.is_error, Some(true));
        assert_eq!(
            unregistered.structured_content.as_ref().unwrap()["kind"],
            "unregistered"
        );

        let mut unresolved = CallToolResult::success(vec![ContentBlock::text("Could not resolve")]);
        promote_upstream_failure(&mut unresolved);
        assert_eq!(unresolved.is_error, Some(false));
    }

    #[tokio::test]
    async fn resolve_query_reports_a_rate_limit_instead_of_a_miss() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "12")
                    .set_body_string("Daily limit reached"),
            )
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .expect(1)
            .mount(&mock)
            .await;

        let err = test_server(&mock.uri())
            .resolve_query_to_metadata("Wright 1931 genetics shifting balance", false)
            .await
            .expect_err("rate limit");
        assert!(err.contains("RATE LIMITED"), "{err}");
        assert!(err.contains("Daily limit reached"), "{err}");
        assert!(err.contains("(Retry-After: 12)"), "{err}");
        assert!(!err.contains("Could not resolve"), "{err}");
        assert!(!err.contains("Not found"), "{err}");
    }

    #[tokio::test]
    async fn group_cite_reports_a_lookup_rate_limit() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "9")
                    .set_body_string("Daily limit reached"),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .group_cite(Parameters(GroupCiteArgs {
                dois: vec!["10.1038/group-cap".into()],
                style: "apa".into(),
            }))
            .await;
        assert!(result.contains("RATE LIMITED"), "{result}");
        assert!(result.contains("(Retry-After: 9)"), "{result}");
        assert!(!result.contains("Group citation failed"), "{result}");
        assert!(!result.contains("Failed to resolve"), "{result}");
    }

    async fn mount_weak_local_reverse(mock: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "metadata": {
                        "title": "Resource allocation based on redundancy models for high availability cloud",
                        "doi": "10.1007/s00607-019-00728-1",
                        "journal": "Computing"
                    },
                    "score": 2.0
                }
            ])))
            .expect(1)
            .mount(mock)
            .await;
    }

    fn live_rate_limit(seconds: &str) -> ResponseTemplate {
        ResponseTemplate::new(429)
            .insert_header("retry-after", seconds)
            .set_body_string("Live lookup quota exhausted")
    }

    #[tokio::test]
    async fn reverse_lookup_keeps_a_live_rate_limit_over_a_weak_hit() {
        let mock = MockServer::start().await;
        mount_weak_local_reverse(&mock).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(live_rate_limit("31"))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Attention Is All You Need Vaswani 2017".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;
        assert!(result.contains("RATE LIMITED"), "{result}");
        assert!(result.contains("Live lookup quota exhausted"), "{result}");
        assert!(result.contains("(Retry-After: 31)"), "{result}");
        assert!(
            !result.contains("Resource allocation based on redundancy models"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn reverse_lookup_reports_a_live_rate_limit_when_local_is_empty() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(live_rate_limit("32"))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .reverse_lookup(Parameters(ReverseArgs {
                text: "A paper the corpus does not hold".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: false,
            }))
            .await;
        assert!(result.contains("RATE LIMITED"), "{result}");
        assert!(result.contains("Live lookup quota exhausted"), "{result}");
        assert!(result.contains("(Retry-After: 32)"), "{result}");
        assert!(!result.contains("No matches"), "{result}");
    }

    #[tokio::test]
    async fn reverse_lookup_reports_a_live_rate_limit_when_live_is_forced() {
        let mock = MockServer::start().await;
        mount_weak_local_reverse(&mock).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(live_rate_limit("33"))
            .expect(1)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Attention Is All You Need Vaswani 2017".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
                use_live_queries: true,
            }))
            .await;
        assert!(result.contains("RATE LIMITED"), "{result}");
        assert!(result.contains("Live lookup quota exhausted"), "{result}");
        assert!(result.contains("(Retry-After: 33)"), "{result}");
        assert!(
            !result.contains("Resource allocation based on redundancy models"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn import_plaintext_outlasts_a_restart_window() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_test");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan": "academic",
                "lookups_remaining": 50,
                "lookups_limit": 60
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/parse-citations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "citations": [{"cleaned_text": "10.1038/restart-import"}]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/restart-import",
                "title": "Stored Once",
                "authors": [{"family": "Ada", "given": "A."}],
                "date": {"year": 2020}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-import", "name": "femtolab", "entry_count": 1}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-import/entries/batch"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-import/entries/batch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: Some("femtolab".into()),
                content: "10.1038/restart-import".into(),
                format: "plaintext".into(),
                style: None,
            }))
            .await;
        assert!(
            result.contains("Imported into 'femtolab': 1 added"),
            "{result}"
        );
        assert!(!result.contains("Import failed"), "{result}");
    }

    #[tokio::test]
    async fn health_check_reports_collections_down_when_a_key_is_set() {
        let _key = EnvGuard::set("OOKCITE_API_KEY", "ookc_test");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "0.1.0"
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "0")
                    .set_body_string("storage down"),
            )
            .expect(3)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .health_check(Parameters(HealthCheckArgs {}))
            .await;
        assert!(result.contains("Status: ok"), "{result}");
        assert!(result.contains("Collections: HTTP 503"), "{result}");
    }

    #[tokio::test]
    async fn health_check_omits_collections_without_a_key() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "0.1.0"
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(0)
            .mount(&mock)
            .await;

        let result = test_server(&mock.uri())
            .health_check(Parameters(HealthCheckArgs {}))
            .await;
        assert!(result.contains("Status: ok"), "{result}");
        assert!(!result.contains("Collections:"), "{result}");
    }

    fn lossy_server_entries(bib: &str) -> Vec<serde_json::Value> {
        let entries = crate::bib_audit::parse_bibtex_raw(bib).expect("fixture parses");
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let doi = entry
                    .fields
                    .iter()
                    .find(|(name, _)| name == "doi")
                    .map(|(_, value)| value.clone());
                let entry_type = match entry.entry_type.as_str() {
                    "manual" | "techreport" | "phdthesis" => "misc",
                    "incollection" => "inbook",
                    "misc" | "online" => "software",
                    "software" => "article",
                    other => other,
                };
                serde_json::json!({
                    "id": format!("e{index}"),
                    "render": {"citation_key": format!("gen{index}ServerKey")},
                    "metadata": {
                        "title": format!("Server title {index}"),
                        "entry_type": entry_type,
                        "authors": [{"family": "Wrong", "given": "Name"}],
                        "citation_key": entry.key,
                        "doi": doi,
                        "issn": "1234-5678",
                        "url": "https://zenodo.org/records/9",
                        "publisher": "Resolver Press",
                        "abstract": "from the resolver",
                        "affiliation": "Resolver Lab",
                        "journal": "Resolver Journal"
                    }
                })
            })
            .collect()
    }

    async fn import_and_export(collection: &str, bib: &str) -> String {
        let mock = MockServer::start().await;
        let col_id = "col-rt";
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": col_id, "name": collection}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/collections/{col_id}/import")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/collections/{col_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": col_id,
                "name": collection,
                "entries": lossy_server_entries(bib)
            })))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let server = test_server(&mock.uri())
            .with_overrides(overrides::OverrideStore::Enabled(dir.path().to_path_buf()));
        let imported = server
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: Some(collection.into()),
                content: bib.into(),
                format: "bibtex".into(),
                style: None,
            }))
            .await;
        let parsed = crate::bib_audit::parse_bibtex_raw(bib).unwrap();
        assert!(
            imported.contains(&format!("Kept {} source entries for export.", parsed.len())),
            "{imported}"
        );
        server
            .export_collection(Parameters(ExportCollectionArgs {
                locale: "en-US".into(),
                collection: collection.into(),
                format: "bib".into(),
                style: "apa".into(),
                software_entry: "misc".into(),
                eid_as: "pages".into(),
                local_overrides: true,
                enrich: false,
            }))
            .await
    }

    fn assert_round_trip(source: &str, exported: &str) {
        let diff = crate::bib_source::diff_bibliographies(source, exported).unwrap();
        assert_eq!(diff.key_changes, 0, "{diff:?}\n{exported}");
        assert_eq!(diff.type_changes, 0, "{diff:?}");
        assert_eq!(diff.dropped_fields, 0, "{diff:?}");
        assert_eq!(diff.added_fields, 0, "{diff:?}");
        assert_eq!(diff.value_changes, 0, "{diff:?}");
        assert_eq!(diff.entries_differing, 0, "{diff:?}");
        assert_eq!(diff.export_entries, diff.source_entries, "{diff:?}");
    }

    fn bibtex_counts(bib: &str, style: &str) -> Option<(usize, usize)> {
        if std::process::Command::new("bibtex")
            .arg("-version")
            .output()
            .is_err()
        {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        std::fs::write(dir.path().join("refs.bib"), bib).ok()?;
        let aux = format!("\\citation{{*}}\n\\bibstyle{{{style}}}\n\\bibdata{{refs}}\n");
        std::fs::write(dir.path().join("paper.aux"), aux).ok()?;
        let output = std::process::Command::new("bibtex")
            .arg("paper")
            .current_dir(dir.path())
            .output()
            .ok()?;
        let blg = std::fs::read_to_string(dir.path().join("paper.blg")).unwrap_or_default();
        let log = format!(
            "{}\n{}\n{blg}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Some((
            bibtex_summary(&log, "error"),
            bibtex_summary(&log, "warning"),
        ))
    }

    fn bibtex_summary(log: &str, kind: &str) -> usize {
        for line in log.lines() {
            let lower = line.to_ascii_lowercase();
            if !(lower.contains("there was") || lower.contains("there were"))
                || !lower.contains(kind)
            {
                continue;
            }
            if let Some(count) = lower
                .split_whitespace()
                .find_map(|word| word.parse::<usize>().ok())
            {
                return count;
            }
        }
        0
    }

    #[test]
    fn kept_source_note_counts_only_what_was_stored() {
        let mut matched = String::new();
        super::Server::append_kept_sources(
            &mut matched,
            &overrides::RecordedSources {
                kept: 5,
                conflicts: Vec::new(),
            },
            5,
        );
        assert_eq!(matched, " Kept 5 source entries for export.");

        let mut missed = String::new();
        super::Server::append_kept_sources(
            &mut missed,
            &overrides::RecordedSources {
                kept: 4,
                conflicts: vec![
                    "Citation key nimbus110 kept the first source; a later entry with that key was not stored."
                        .into(),
                ],
            },
            5,
        );
        assert!(missed.contains("Kept 4 source entries"), "{missed}");
        assert!(
            missed.contains("1 imported entries were not stored."),
            "{missed}"
        );
        assert!(missed.contains("Citation key nimbus110"), "{missed}");

        let mut extra = String::new();
        super::Server::append_kept_sources(
            &mut extra,
            &overrides::RecordedSources {
                kept: 2,
                conflicts: Vec::new(),
            },
            1,
        );
        assert!(
            extra.contains("0 imported entries were not stored."),
            "{extra}"
        );
    }

    fn kept_count(message: &str) -> usize {
        let rest = message
            .split("Kept ")
            .nth(1)
            .expect("import message names the kept count");
        rest.split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn import_keeps_every_source_and_the_count_matches_the_store() {
        let bib = "\
@article{quartzFmt2017,
  title = {Quartz\u{2014}An efficient synthetic format},
  year = {2017},
  doi = {10.5555/synth.quartz-fmt}
}
@article{pebbleSet2020,
  title = {PebbleSet and Toolset\u{2014}Lightweight},
  year = {2020},
  doi = {10.5555/synth.pebble-set}
}
@misc{nimbusRead2026,
  title = {Nimbus\u{2014}a fast reader},
  year = {2026},
  doi = {10.5555/synth.nimbus-read}
}
@misc{nimbusHowtoLattice2026,
  title = {{Nimbus} Documentation},
  author = {{Nimbus developers}},
  year = {2026},
  howpublished = {\\url{https://example.org/Howto_lattice.html}}
}
@manual{nimbus110,
  author = {{Nimbus developers}},
  title = {Nimbus format specification version 1.1.0},
  version = {1.1.0},
  url = {https://example.org/releases/nimbus-1.1.0.html}
}
";
        let mock = MockServer::start().await;
        let col_id = "col-rt";
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": col_id, "name": "manuals"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/collections/{col_id}/import")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 5,
                "duplicates_skipped": 0
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/collections/{col_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": col_id,
                "name": "manuals",
                "entries": [
                    {
                        "id": "e-quartz",
                        "metadata": {
                            "doi": "10.5555/synth.quartz-fmt",
                            "title": "Quartz---An efficient synthetic format",
                            "entry_type": "article",
                            "year": 2017
                        }
                    },
                    {
                        "id": "e-pebble",
                        "metadata": {
                            "doi": "10.5555/synth.pebble-set",
                            "title": "PebbleSet and Toolset---Lightweight",
                            "entry_type": "article",
                            "year": 2020
                        }
                    },
                    {
                        "id": "e-nimbus-read",
                        "metadata": {
                            "doi": "10.5555/synth.nimbus-read",
                            "title": "Nimbus---a fast reader",
                            "entry_type": "misc",
                            "year": 2026
                        }
                    },
                    {
                        "id": "e-nimbus-doc",
                        "metadata": {
                            "citation_key": "nimbusHowtoLattice2026",
                            "title": "Nimbus Documentation",
                            "entry_type": "misc",
                            "year": 2026
                        }
                    },
                    {
                        "id": "nimbusdevelopersNimbusFormatSpecification",
                        "render": {"citation_key": "nimbusdevelopersNimbusFormatSpecification"},
                        "metadata": {
                            "title": "Nimbus format specification version 1.1.0",
                            "entry_type": "article",
                            "authors": [{"family": "Nimbus developers"}],
                            "url": "https://example.org/releases/nimbus-1.1.0.html"
                        }
                    }
                ]
            })))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let server = test_server(&mock.uri())
            .with_overrides(overrides::OverrideStore::Enabled(dir.path().to_path_buf()));
        let imported = server
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: Some("manuals".into()),
                content: bib.into(),
                format: "bibtex".into(),
                style: None,
            }))
            .await;
        let store = overrides::load_from(dir.path(), "manuals").unwrap();
        let mut stored_keys = Vec::new();
        for entry in store.entries.values() {
            if let Some(source) = &entry.source {
                if !stored_keys.contains(&source.key) {
                    stored_keys.push(source.key.clone());
                }
            }
        }
        let exported = server
            .export_collection(Parameters(ExportCollectionArgs {
                locale: "en-US".into(),
                collection: "manuals".into(),
                format: "bib".into(),
                style: "apa".into(),
                software_entry: "misc".into(),
                eid_as: "pages".into(),
                local_overrides: true,
                enrich: false,
            }))
            .await;
        let mut problems = Vec::new();
        let kept = kept_count(&imported);
        if kept != stored_keys.len() {
            problems.push(format!(
                "kept count {kept} != stored keys {stored_keys:?} in {imported}"
            ));
        }
        if !stored_keys.iter().any(|key| key == "nimbus110") {
            problems.push(format!("nimbus110 was not stored: {stored_keys:?}"));
        }
        for needle in [
            "@manual{nimbus110,",
            "author = {{Nimbus developers}}",
            "version = {1.1.0}",
            "url = {https://example.org/releases/nimbus-1.1.0.html}",
            "Quartz\u{2014}An efficient",
            "Toolset\u{2014}Lightweight",
            "Nimbus\u{2014}a fast reader",
            r"\url{https://example.org/Howto_lattice.html}",
        ] {
            if !exported.contains(needle) {
                problems.push(format!("export missing {needle}"));
            }
        }
        if exported.contains("nimbusdevelopersNimbusFormatSpecification") {
            problems.push("export rekeyed nimbus110".into());
        }
        if exported.contains("---") {
            problems.push("export rewrote an em dash to ---".into());
        }
        if exported.contains(r"Howto\_lattice") {
            problems.push("export escaped an underscore inside \\url".into());
        }
        assert!(problems.is_empty(), "{problems:#?}\n{exported}");
    }

    #[tokio::test]
    async fn export_says_when_an_entry_has_no_kept_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut planted = overrides::CollectionOverrides::new("Refs");
        planted.entries.insert(
            "key:other".into(),
            overrides::EntryOverride {
                source: Some(crate::bib_source::BibSource {
                    key: "other".into(),
                    entry_type: "article".into(),
                    fields: vec![("title".into(), "Unrelated specification".into())],

                    separators: Vec::new(),
                }),
                ..Default::default()
            },
        );
        overrides::save_to(dir.path(), "Refs", &planted).unwrap();
        let mock = MockServer::start().await;
        mount_refs_collection(&mock).await;
        let server = test_server(&mock.uri())
            .with_overrides(overrides::OverrideStore::Enabled(dir.path().to_path_buf()));
        let bib = server.export_collection(Parameters(export_args())).await;
        assert!(bib.contains("no kept source"), "{bib}");
        assert!(bib.contains("Re-import"), "{bib}");
        assert!(!bib.contains("Unrelated specification"), "{bib}");
        assert!(bib.contains("@article{"), "{bib}");
    }

    #[tokio::test]
    async fn import_then_export_keeps_the_source_bibliography() {
        let bib = include_str!("../tests/fixtures/roundtrip-shapes.bib");
        let exported = import_and_export("shapes", bib).await;
        assert_eq!(exported, bib, "{exported}");
        assert_round_trip(bib, &exported);
        assert!(exported.contains("@incollection{voter2007,"), "{exported}");
        assert!(exported.contains("@manual{h5md110,"), "{exported}");
        assert!(exported.contains("@misc{ieee754,"), "{exported}");
        assert!(exported.contains("@software{chemfiles2023,"), "{exported}");
        assert!(exported.contains("De Souza, Roger A."), "{exported}");
        assert!(exported.contains("Le Bris, Claude"), "{exported}");
        assert!(exported.contains("S{\\o}rensen, M. R."), "{exported}");
        assert!(exported.contains("pages = {23-32}"), "{exported}");
        assert!(exported.contains("pages = {134106}"), "{exported}");
        assert!(
            exported.contains("https://github.com/aldanads/Kinetix"),
            "{exported}"
        );
        assert!(!exported.contains("@inbook"), "{exported}");
        assert!(!exported.contains("issn"), "{exported}");
        assert!(!exported.contains("zenodo.org"), "{exported}");
        assert!(!exported.contains("Resolver"), "{exported}");
        assert!(!exported.contains("Server title"), "{exported}");
        for style in ["elsarticle-num", "aipnum4-2"] {
            let Some((source_errors, source_warnings)) = bibtex_counts(bib, style) else {
                continue;
            };
            let (errors, warnings) = bibtex_counts(&exported, style).unwrap();
            assert_eq!(
                errors, 0,
                "{style} export errors, source had {source_errors}"
            );
            assert!(
                warnings <= source_warnings,
                "{style} export warnings {warnings} exceed the source {source_warnings}"
            );
        }
    }

    #[tokio::test]
    async fn shared_server_import_does_not_keep_the_source() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-rt", "name": "shapes"}
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/collections/col-rt/import"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "added": 1,
                "duplicates_skipped": 0
            })))
            .mount(&mock)
            .await;
        let bib = include_str!("../tests/fixtures/roundtrip-shapes.bib");
        let imported = test_server(&mock.uri())
            .import_bibliography(Parameters(ImportBibliographyArgs {
                locale: "en-US".into(),
                collection: Some("shapes".into()),
                content: bib.into(),
                format: "bibtex".into(),
                style: None,
            }))
            .await;
        assert!(
            imported.contains("does not keep a local copy"),
            "{imported}"
        );
        assert!(!imported.contains("Kept "), "{imported}");
    }

    #[tokio::test]
    async fn private_bibliographies_round_trip_when_configured() {
        let Ok(dir) = std::env::var("OOKCITE_PRIVATE_BIB_DIR") else {
            return;
        };
        let root = std::path::PathBuf::from(dir);
        if !root.is_dir() {
            return;
        }
        let mut files = Vec::new();
        collect_bib_files(&root, &mut files);
        files.sort();
        assert!(
            !files.is_empty(),
            "OOKCITE_PRIVATE_BIB_DIR has no BibTeX files"
        );
        for path in files {
            let source = std::fs::read_to_string(&path).unwrap();
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("bibliography")
                .to_string();
            let exported = import_and_export(&label, &source).await;
            let diff = crate::bib_source::diff_bibliographies(&source, &exported).unwrap();
            println!(
                "{label}: entries {} key {} type {} dropped {} added {} value {} differing {}",
                diff.source_entries,
                diff.key_changes,
                diff.type_changes,
                diff.dropped_fields,
                diff.added_fields,
                diff.value_changes,
                diff.entries_differing
            );
            assert_eq!(diff.key_changes, 0, "{label} {diff:?}");
            assert_eq!(diff.type_changes, 0, "{label} {diff:?}");
            assert_eq!(diff.dropped_fields, 0, "{label} {diff:?}");
            assert_eq!(diff.added_fields, 0, "{label} {diff:?}");
            assert_eq!(diff.value_changes, 0, "{label} {diff:?}");
            assert_eq!(diff.entries_differing, 0, "{label} {diff:?}");
            assert_eq!(diff.export_entries, diff.source_entries, "{label} {diff:?}");
            for style in ["elsarticle-num", "aipnum4-2"] {
                let Some((source_errors, source_warnings)) = bibtex_counts(&source, style) else {
                    continue;
                };
                let (errors, warnings) = bibtex_counts(&exported, style).unwrap();
                assert_eq!(
                    errors, 0,
                    "{label} {style} errors, source had {source_errors}"
                );
                assert!(
                    warnings <= source_warnings,
                    "{label} {style} warnings {warnings} exceed the source {source_warnings}"
                );
            }
        }
    }

    fn collect_bib_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_bib_files(&path, out);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("bib"))
            {
                out.push(path);
            }
        }
    }
}
