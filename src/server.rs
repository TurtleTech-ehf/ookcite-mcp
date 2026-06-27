//! MCP server: tool router, HTTP client, and handlers.

use futures::{stream, StreamExt};
use ookcite_mcp::endpoints::{self, Endpoint};
use rmcp::ServerHandler;
use rmcp::{
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use crate::collection_entries::{
    entry_doi, format_collection_entry_line, looks_like_doi_token, resolve_entry_id_in_collection,
};
use crate::batch_limits::{
    collect_dois_from_collection_body, format_member_valid_lines, plan_metered_batch,
    read_only_concurrency, DoiResponseCache, MeQuota,
};
use crate::constants::{
    api_base_url, build_api_client, setup_help_block, MUTATE_BATCH_CONCURRENCY,
    MIN_CONFIDENT_REVERSE_LOOKUP_SCORE,
};
use crate::http_error::{
    classify_collection_create_failure, classify_lookup_doi_failure, error_detail,
};
use crate::policy::{self, block_mutate};
use crate::resolve_helpers::{
    classify_reverse_lookup_response, format_resolve_candidates, lookup_doi_with_retry,
    resolve_payload_metadata, resolve_text_body, reverse_lookup_resolve_body,
};
use crate::tool_args::*;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Process-wide identity-safe exact-DOI cache (shared across Server clones).
fn shared_doi_cache() -> &'static DoiResponseCache {
    static CACHE: OnceLock<DoiResponseCache> = OnceLock::new();
    CACHE.get_or_init(DoiResponseCache::with_default_ttl)
}

#[derive(Clone)]
pub struct Server {
    tool_router: ToolRouter<Self>,
    http: reqwest::Client,
    api_base: String,
    doi_cache: DoiResponseCache,
}

#[tool_router]
impl Server {
    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();

        if let Ok(api_key) = std::env::var("OOKCITE_API_KEY") {
            if let Ok(mut auth_val) =
                format!("Bearer {api_key}").parse::<reqwest::header::HeaderValue>()
            {
                auth_val.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, auth_val);
            }
        }
        // Anonymous notice is emitted once from main when probes are off.

        Self {
            tool_router: Self::tool_router(),
            http: build_api_client(30, headers),
            api_base: api_base_url(),
            doi_cache: shared_doi_cache().clone(),
        }
    }

    async fn fetch_me_quota(&self) -> Option<MeQuota> {
        if std::env::var("OOKCITE_API_KEY").is_err() {
            return None;
        }
        let resp = self.request(endpoints::ME, &[]).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        Some(MeQuota::from_json(&v))
    }

    /// Load DOIs from all collections (bounded: first page of list + each GET).
    /// Used for upfront membership so collection-local DOIs skip metered fan-out.
    async fn load_collection_doi_membership(
        &self,
    ) -> (HashSet<String>, HashMap<String, String>) {
        let mut dois = HashSet::new();
        let mut titles = HashMap::new();
        if std::env::var("OOKCITE_API_KEY").is_err() {
            return (dois, titles);
        }
        let Ok(resp) = self.request(endpoints::COLLECTIONS_LIST, &[]).send().await else {
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
                .request(endpoints::COLLECTION_GET, &[("id", id)])
                .send()
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

    async fn lookup_doi_json_cached(&self, doi: &str) -> Result<serde_json::Value, String> {
        if let Some(meta) = self.doi_cache.get_valid(doi) {
            return Ok(meta);
        }
        let r = lookup_doi_with_retry(&self.http, &self.api_base, doi).await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let meta: serde_json::Value = resp.json().await.unwrap_or_default();
                self.doi_cache.put_if_identity_ok(doi, meta.clone());
                Ok(meta)
            }
            Ok(resp) => Err(classify_lookup_doi_failure(resp, doi).await),
            Err(e) => Err(format!("ERROR {doi} : {e}")),
        }
    }

    /// Collection-local metadata only — never hits LOOKUP_DOI (free tier path).
    fn synthetic_member_metadata(doi: &str, title: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "doi": doi,
            "title": title.unwrap_or("(in your collection)"),
            "entry_type": "article",
            "authors": [],
        })
    }

    /// Preflight + membership for multi-DOI tools; metered DOIs use cache then API.
    async fn resolve_dois_with_preflight(
        &self,
        dois: &[String],
    ) -> Result<Vec<serde_json::Value>, String> {
        let has_key = std::env::var("OOKCITE_API_KEY").is_ok();
        let quota = self.fetch_me_quota().await;
        let (member_dois, member_titles) = if has_key {
            self.load_collection_doi_membership().await
        } else {
            (HashSet::new(), HashMap::new())
        };
        let pf = plan_metered_batch(
            dois,
            &member_dois,
            &member_titles,
            quota.as_ref(),
            has_key,
        );
        if let Some(msg) = pf.refuse_message {
            return Err(msg);
        }
        let mut entries: Vec<serde_json::Value> = pf
            .members
            .iter()
            .map(|(doi, title)| Self::synthetic_member_metadata(doi, title.as_deref()))
            .collect();
        let conc = read_only_concurrency();
        let futs: Vec<_> = pf
            .need_lookup
            .iter()
            .map(|doi| {
                let server = self.clone();
                let doi = doi.clone();
                async move { server.lookup_doi_json_cached(&doi).await.ok() }
            })
            .collect();
        let looked: Vec<_> = stream::iter(futs)
            .buffer_unordered(conc)
            .collect::<Vec<_>>()
            .await;
        entries.extend(looked.into_iter().flatten());
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

        let health = self.request(endpoints::HEALTH, &[]).send().await;
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

        if std::env::var("OOKCITE_API_KEY").is_ok() {
            match self.request(endpoints::ME, &[]).send().await {
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
            out.push(
                "Auth /me: skipped (no OOKCITE_API_KEY; anonymous IP limits apply)".into(),
            );
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
        match ep.method {
            "GET" => self.http.get(url),
            "POST" => self.http.post(url),
            "PATCH" => self.http.patch(url),
            "DELETE" => self.http.delete(url),
            other => panic!(
                "ookcite-mcp: unsupported HTTP method `{other}` in registry for {}",
                ep.path
            ),
        }
    }

    #[tool(
        name = "search_styles",
        description = "Search for available CSL citation styles by name. Returns a list of matching style IDs to use in formatting tools.",
        annotations(title = "Search CSL styles", read_only_hint = true, idempotent_hint = true)
    )]
    async fn search_styles(&self, Parameters(args): Parameters<StyleSearchArgs>) -> String {
        let r = self
            .request(endpoints::STYLES_SEARCH, &[])
            .query(&[("q", args.query.as_str())])
            .send()
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
            _ => "Style search failed".into(),
        }
    }

    #[tool(
        name = "validate_doi",
        description = "Check if a DOI exists and return its metadata. Use this to verify citations are real. Returns title, authors, year, journal, volume, and issue. Prefer verify_references for multiple DOIs in one call.",
        annotations(title = "Validate DOI", read_only_hint = true, idempotent_hint = true)
    )]
    async fn validate_doi(&self, Parameters(args): Parameters<DoiArgs>) -> String {
        match self.lookup_doi_json_cached(&args.doi).await {
            Ok(meta) => {
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
                let journal = meta["journal"].as_str().unwrap_or("N/A");
                let volume = meta["volume"].as_str().unwrap_or("N/A");
                let issue = meta["issue"].as_str().unwrap_or("N/A");
                let doi = meta["doi"].as_str().unwrap_or(&args.doi);
                format!(
                    "VALID\nDOI: {doi}\nTitle: {title}\nAuthors: {authors}\nYear: {year}\nJournal: {journal}\nVolume: {volume}\nIssue: {issue}"
                )
            }
            Err(msg) => msg,
        }
    }

    #[tool(
        name = "lookup_isbn",
        description = "Look up a book by ISBN. Returns title, authors, publisher, year, and pages.",
        annotations(title = "Lookup ISBN", read_only_hint = true, idempotent_hint = true)
    )]
    async fn lookup_isbn(&self, Parameters(args): Parameters<IsbnArgs>) -> String {
        let r = self
            .request(endpoints::LOOKUP_ISBN, &[])
            .json(&serde_json::json!({"isbn": args.isbn}))
            .send()
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
                format!("RATE LIMITED: {}", error_detail(r).await)
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
        description = "Parse a messy citation string and find the matching paper. Searches the local corpus first, then retries with live upstream providers when local search is empty or weak. Set use_live_queries=true to allow live upstream providers on the first pass. Optional filters (author, journal, year, orcid) boost matching results. For many citations prefer batch_format.",
        annotations(title = "Reverse lookup citation", read_only_hint = true, idempotent_hint = true)
    )]
    async fn reverse_lookup(&self, Parameters(args): Parameters<ReverseArgs>) -> String {
        let body = reverse_lookup_resolve_body(&args, args.use_live_queries);
        let r = self
            .request(endpoints::RESOLVE, &[])
            .json(&body)
            .send()
            .await;
        match classify_reverse_lookup_response(r).await {
            Ok(Some(local_match))
                if !args.use_live_queries
                    && local_match.top_score < MIN_CONFIDENT_REVERSE_LOOKUP_SCORE =>
            {
                let live_body = reverse_lookup_resolve_body(&args, true);
                let live = self
                    .request(endpoints::RESOLVE, &[])
                    .json(&live_body)
                    .send()
                    .await;
                match classify_reverse_lookup_response(live).await {
                    Ok(Some(live_match))
                        if live_match.top_score >= MIN_CONFIDENT_REVERSE_LOOKUP_SCORE =>
                    {
                        live_match.output
                    }
                    Ok(Some(_)) | Ok(None) => "No confident matches found".into(),
                    Err(_) => local_match.output,
                }
            }
            Ok(Some(local_match)) => local_match.output,
            Ok(None) if !args.use_live_queries => {
                let live_body = reverse_lookup_resolve_body(&args, true);
                let live = self
                    .request(endpoints::RESOLVE, &[])
                    .json(&live_body)
                    .send()
                    .await;
                match classify_reverse_lookup_response(live).await {
                    Ok(Some(live_match)) => live_match.output,
                    Ok(None) => "No matches found".into(),
                    Err(message) => message,
                }
            }
            Ok(None) => "No matches found".into(),
            Err(message) => message,
        }
    }

    #[tool(
        name = "parse_citations",
        description = "Parse raw bibliography text into structured citation units. Splits multi-citation blocks, extracts DOIs/ISBNs, and provides title/author/year hints. Use this to break down pasted bibliographies before resolving individual citations.",
        annotations(title = "Parse bibliography text", read_only_hint = true, idempotent_hint = true)
    )]
    async fn parse_citations(&self, Parameters(args): Parameters<ParseCitationsArgs>) -> String {
        let r = self
            .request(endpoints::PARSE_CITATIONS, &[])
            .json(&serde_json::json!({"text": args.text}))
            .send()
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
                format!("RATE LIMITED: {}", error_detail(r).await)
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
        annotations(title = "Debug citation resolve", read_only_hint = true, idempotent_hint = true)
    )]
    async fn debug_resolve(&self, Parameters(args): Parameters<DebugResolveArgs>) -> String {
        let r = self
            .request(endpoints::RESOLVE_DEBUG, &[])
            .json(&serde_json::json!({
                "input": {"text": args.text}
            }))
            .send()
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
                format!("RATE LIMITED: {}", error_detail(r).await)
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
        description = "Format a citation by DOI in a specific CSL style. Returns both the in-text marker and the full bibliography entry. Prefer batch_format for multiple citations.",
        annotations(title = "Format citation", read_only_hint = true, idempotent_hint = true)
    )]
    async fn format_citation(&self, Parameters(args): Parameters<FormatArgs>) -> String {
        let lookup = self
            .request(endpoints::LOOKUP_DOI, &[])
            .json(&serde_json::json!({"doi": args.doi}))
            .send()
            .await;
        let meta: serde_json::Value = match lookup {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            Ok(r) if r.status().as_u16() == 429 => {
                return format!("RATE LIMITED: {}", error_detail(r).await);
            }
            Ok(r) if r.status().as_u16() == 403 => {
                return format!("ACCESS DENIED: {}", error_detail(r).await);
            }
            Ok(_) => return format!("DOI {} not found", args.doi),
            Err(e) => return format!("ERROR: {e}"),
        };

        let fmt = self
            .request(endpoints::FORMAT, &[])
            .json(&serde_json::json!({"entries": [meta], "style": args.style, "locale": "en-US"}))
            .send()
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
                format!("In-text: {intext}\nReference: {plain}")
            }
            _ => "Format failed".into(),
        }
    }

    #[tool(
        name = "group_cite",
        description = "Generate a grouped in-text citation marker (e.g., '[1-3]') for multiple DOIs.",
        annotations(title = "Group in-text cites", read_only_hint = true, idempotent_hint = true)
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
            .request(endpoints::FORMAT_GROUP_CITE, &[])
            .json(&serde_json::json!({
                "entries": entries,
                "indices": indices,
                "style": args.style
            }))
            .send()
            .await;

        match r {
            Ok(resp) if resp.status().is_success() => {
                let result: serde_json::Value = resp.json().await.unwrap_or_default();
                let plain = result["plain"].as_str().unwrap_or("");
                format!("Grouped Citation: {plain}")
            }
            _ => "Group citation failed".into(),
        }
    }

    #[tool(
        name = "verify_references",
        description = "Batch verify that a list of DOIs exist. Returns VALID or INVALID for each. Checks daily quota and collection membership upfront so oversized batches refuse before burning lookups; collection members are reported without a metered call. Prefer over repeated validate_doi.",
        annotations(title = "Verify DOIs (batch)", read_only_hint = true, idempotent_hint = true)
    )]
    async fn verify_references(&self, Parameters(args): Parameters<VerifyArgs>) -> String {
        let has_key = std::env::var("OOKCITE_API_KEY").is_ok();
        let quota = self.fetch_me_quota().await;
        let (member_dois, member_titles) = if has_key {
            self.load_collection_doi_membership().await
        } else {
            (HashSet::new(), HashMap::new())
        };
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
        let mut results = format_member_valid_lines(&pf.members);
        let conc = read_only_concurrency();
        let futs: Vec<_> = pf
            .need_lookup
            .iter()
            .map(|doi| {
                let server = self.clone();
                let doi = doi.clone();
                async move {
                    match server.lookup_doi_json_cached(&doi).await {
                        Ok(meta) => {
                            let title = meta["title"].as_str().unwrap_or("?");
                            format!("VALID {doi} : {title}")
                        }
                        Err(e) => e,
                    }
                }
            })
            .collect();
        let looked_up = stream::iter(futs)
            .buffer_unordered(conc)
            .collect::<Vec<_>>()
            .await;
        results.extend(looked_up);
        results.join("\n")
    }

    #[tool(
        name = "batch_format",
        description = "Resolve and format multiple messy citations at once. Checks quota upfront for DOI-shaped items and prefers collection membership; uses the local corpus by default. Prefer over N× reverse_lookup + format_citation. Import large bibliographies into a collection for free revisits.",
        annotations(title = "Batch format citations", read_only_hint = true, idempotent_hint = true)
    )]
    async fn batch_format(&self, Parameters(args): Parameters<BatchArgs>) -> String {
        let has_key = std::env::var("OOKCITE_API_KEY").is_ok();
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
        // Collection members: synthetic metadata only — never LOOKUP_DOI (free path).
        let mut entries: Vec<serde_json::Value> = pf
            .members
            .iter()
            .map(|(doi, title)| Self::synthetic_member_metadata(doi, title.as_deref()))
            .collect();

        let conc = read_only_concurrency();
        let futs: Vec<_> = pf
            .need_lookup
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let server = self.clone();
                let text = text.clone();
                async move {
                    if looks_like_doi_token(&text) {
                        if let Ok(meta) = server.lookup_doi_json_cached(&text).await {
                            return Ok(meta);
                        }
                    }
                    if let Some(meta) = server
                        .resolve_query_to_metadata(&text, use_live_queries)
                        .await
                    {
                        Ok(meta)
                    } else {
                        Err(format!(
                            "[{}] Not found: {}",
                            i + 1,
                            &text[..text.len().min(60)]
                        ))
                    }
                }
            })
            .collect();
        let resolved: Vec<_> = stream::iter(futs).buffer_unordered(conc).collect().await;

        let mut errors = Vec::new();
        for result in resolved {
            match result {
                Ok(meta) => entries.push(meta),
                Err(e) => errors.push(e),
            }
        }

        if entries.is_empty() {
            return format!("No citations resolved.\n{}", errors.join("\n"));
        }
        let fmt = self
            .request(endpoints::FORMAT, &[])
            .json(&serde_json::json!({"entries": entries, "style": args.style, "locale": "en-US"}))
            .send()
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
            Ok(r) => format!("Batch format failed: HTTP {}", r.status()),
            Err(e) => format!("Batch format failed: {e}"),
        }
    }

    #[tool(
        name = "list_collections",
        description = "List all citation collections for the authenticated user. Requires OOKCITE_API_KEY.",
        annotations(title = "List collections", read_only_hint = true, idempotent_hint = true)
    )]
    async fn list_collections(
        &self,
        #[allow(unused)] Parameters(_args): Parameters<ListCollectionsArgs>,
    ) -> String {
        let r = self.request(endpoints::COLLECTIONS_LIST, &[]).send().await;
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
            Ok(r) if r.status().as_u16() == 401 => {
                "Authentication required. Set OOKCITE_API_KEY.".into()
            }
            Ok(r) if r.status().as_u16() == 503 => {
                "Collections not available (S3 not configured).".into()
            }
            _ => "Failed to list collections.".into(),
        }
    }

    #[tool(
        name = "add_to_collection",
        description = "Add a citation to a collection. Searches by DOI, ISBN, or free-text (e.g. 'Goswami JCTC 2026'). Creates the collection if it doesn't exist. Free-text uses the local corpus by default; set use_live_queries=true to allow live upstream provider calls. Prefer batch_add_to_collection for multiple items.",
        annotations(title = "Add to collection", read_only_hint = false, destructive_hint = false, idempotent_hint = false)
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
                match self
                    .resolve_query_to_metadata(q, args.use_live_queries)
                    .await
                {
                    Some(metadata) => metadata,
                    None => return format!("Could not resolve: {}", args.query),
                }
            } else {
                let resolve = self
                    .request(endpoints::RESOLVE, &[])
                    .json(&resolve_text_body(q, args.use_live_queries))
                    .send()
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
                            match self
                                .resolve_query_to_metadata(q, args.use_live_queries)
                                .await
                            {
                                Some(metadata) => metadata,
                                None => return format!("Could not resolve: {}", args.query),
                            }
                        } else {
                            match self
                                .resolve_query_to_metadata(q, args.use_live_queries)
                                .await
                            {
                                Some(metadata) => metadata,
                                None => return format!("Could not resolve: {}", args.query),
                            }
                        }
                    }
                    _ => match self
                        .resolve_query_to_metadata(q, args.use_live_queries)
                        .await
                    {
                        Some(metadata) => metadata,
                        None => return format!("Could not resolve: {}", args.query),
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
            Ok(r) => format!("Failed to add entry: {}", error_detail(r).await),
            Err(e) => format!("Failed to add entry: {e}"),
        }
    }

    #[tool(
        name = "export_collection",
        description = "Export a collection as BibTeX. Returns the full .bib file content with Better BibTeX keys.",
        annotations(title = "Export collection BibTeX", read_only_hint = true, idempotent_hint = true)
    )]
    async fn export_collection(
        &self,
        Parameters(args): Parameters<ExportCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let r = self
            .request(endpoints::COLLECTION_EXPORT_BIB, &[("id", &col_id)])
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                r.text().await.unwrap_or_else(|_| "Export failed.".into())
            }
            _ => "Failed to export collection.".into(),
        }
    }

    #[tool(
        name = "search_collection",
        description = "Search within a collection by author name, title keywords, or journal. Returns matching entries with entry_id values for remove_from_collection (opaque id, or pass a bare DOI / doi:10.x/y alias).",
        annotations(title = "Search collection", read_only_hint = true, idempotent_hint = true)
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
            .request(endpoints::COLLECTION_GET, &[("id", &col_id)])
            .send()
            .await;
        let collection: serde_json::Value = match r {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => return "Failed to load collection.".into(),
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
    async fn resolve_collection_entry_id(
        &self,
        col_id: &str,
        needle: &str,
    ) -> Result<String, String> {
        let r = self
            .request(endpoints::COLLECTION_GET, &[("id", col_id)])
            .send()
            .await;
        let collection: serde_json::Value = match r {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            Ok(r) => {
                return Err(format!(
                    "Failed to load collection for entry lookup: {}",
                    error_detail(r).await
                ));
            }
            Err(e) => return Err(format!("Failed to load collection for entry lookup: {e}")),
        };
        let entries = collection["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
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
            match self.request(endpoints::COLLECTIONS_LIST, &[]).send().await {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                Ok(r) if r.status().as_u16() == 401 => {
                    return Err("Authentication required. Set OOKCITE_API_KEY.".into());
                }
                Ok(r) => {
                    return Err(format!(
                        "Failed to list collections: {}",
                        error_detail(r).await
                    ));
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

    async fn resolve_query_to_metadata(
        &self,
        query: &str,
        use_live_queries: bool,
    ) -> Option<serde_json::Value> {
        let q = query.trim();
        if q.starts_with("10.") {
            let r = self
                .request(endpoints::LOOKUP_DOI, &[])
                .json(&serde_json::json!({"doi": q}))
                .send()
                .await;
            match r {
                Ok(r) if r.status().is_success() => {
                    Some(r.json::<serde_json::Value>().await.unwrap_or_default())
                }
                _ => None,
            }
        } else {
            let resolve = self
                .request(endpoints::RESOLVE, &[])
                .json(&resolve_text_body(q, use_live_queries))
                .send()
                .await;
            match resolve {
                Ok(r) if r.status().is_success() => {
                    let payload: serde_json::Value = r.json().await.unwrap_or_default();
                    if let Some(metadata) = resolve_payload_metadata(&payload) {
                        return Some(metadata);
                    }
                }
                _ => {}
            }

            if !use_live_queries {
                return None;
            }

            let reverse = self
                .request(endpoints::REVERSE, &[])
                .json(&serde_json::json!({"text": q}))
                .send()
                .await;
            match reverse {
                Ok(r) if r.status().is_success() => {
                    let results: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                    results.first().and_then(|r| r.get("metadata")).cloned()
                }
                _ => None,
            }
        }
    }

    // --- Phase 1: High-value new tools ---

    #[tool(
        name = "health_check",
        description = "Check if the OokCite API is reachable and healthy. Returns service status and cache statistics.",
        annotations(title = "API health check", read_only_hint = true, idempotent_hint = true)
    )]
    async fn health_check(
        &self,
        #[allow(unused)] Parameters(_args): Parameters<HealthCheckArgs>,
    ) -> String {
        let r = self.request(endpoints::HEALTH, &[]).send().await;
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
        annotations(title = "ookcite doctor", read_only_hint = true, idempotent_hint = true)
    )]
    async fn doctor(
        &self,
        #[allow(unused)] Parameters(_args): Parameters<HealthCheckArgs>,
    ) -> String {
        self.doctor_report().await
    }

    #[tool(
        name = "import_bibliography",
        description = "Import a BibTeX (.bib) or RIS file into a collection. Pass the file content as a string. Creates the collection if it doesn't exist, but collection import may require a paid plan or additional collection capacity.",
        annotations(title = "Import bibliography file", read_only_hint = false, destructive_hint = false, idempotent_hint = false)
    )]
    async fn import_bibliography(
        &self,
        Parameters(args): Parameters<ImportBibliographyArgs>,
    ) -> String {
        if let Some(msg) = block_mutate() {
            return msg;
        }
        let col_id = match self.resolve_or_create_collection(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let filename = if args.format == "ris" {
            "import.ris"
        } else {
            "import.bib"
        };
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
                format!(
                    "Imported into '{}': {added} added, {dupes} duplicates skipped",
                    args.collection
                )
            }
            Ok(r) if r.status().as_u16() == 401 => {
                "Authentication required. Set OOKCITE_API_KEY.".into()
            }
            Ok(r) => format!("Import failed: {}", error_detail(r).await),
            Err(e) => format!("Import failed: {e}"),
        }
    }

    #[tool(
        name = "check_duplicates",
        description = "Check if a citation already exists in a collection. Resolves the query first, then checks for duplicates. Returns entry_id for matches.",
        annotations(title = "Check collection duplicates", read_only_hint = true, idempotent_hint = true)
    )]
    async fn check_duplicates(&self, Parameters(args): Parameters<CheckDuplicatesArgs>) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let Some(metadata) = self
            .resolve_query_to_metadata(&args.query, args.use_live_queries)
            .await
        else {
            return format!("Could not resolve: {}", args.query);
        };

        let r = self
            .request(endpoints::COLLECTION_CHECK_DUPLICATES, &[("id", &col_id)])
            .json(&serde_json::json!({"metadata": metadata}))
            .send()
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
            _ => "Duplicate check failed.".into(),
        }
    }

    #[tool(
        name = "batch_add_to_collection",
        description = "Add multiple citations to a collection at once. Each query can be a DOI or free-text search. Creates the collection if it doesn't exist, but batch collection workflows may require a paid plan or additional collection capacity. Free-text uses the local corpus by default; set use_live_queries=true to allow live upstream provider calls. Prefer over N× add_to_collection.",
        annotations(title = "Batch add to collection", read_only_hint = false, destructive_hint = false, idempotent_hint = false)
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
                    let meta = server.resolve_query_to_metadata(q, use_live_queries).await;
                    match meta {
                        Some(m) => Ok(m),
                        None => Err(format!(
                            "[{}] Could not resolve: {}",
                            i + 1,
                            &query[..query.len().min(60)]
                        )),
                    }
                }
            })
            .collect();
        let resolved: Vec<_> = stream::iter(futs)
            .buffer_unordered(MUTATE_BATCH_CONCURRENCY)
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

        if entries.is_empty() {
            return format!("No citations resolved.\n{}", errors.join("\n"));
        }

        let r = self
            .request(endpoints::COLLECTION_ENTRIES_BATCH, &[("id", &col_id)])
            .json(&serde_json::json!({"entries": entries}))
            .send()
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
                if !errors.is_empty() {
                    out.push_str(&format!("\n\nUnresolved:\n{}", errors.join("\n")));
                }
                out
            }
            Ok(r) => format!("Batch add failed: {}", error_detail(r).await),
            Err(e) => format!("Batch add failed: {e}"),
        }
    }

    // --- Phase 2: Collection management ---

    #[tool(
        name = "delete_collection",
        description = "Delete a citation collection. This is irreversible.",
        annotations(title = "Delete collection", read_only_hint = false, destructive_hint = true, idempotent_hint = false)
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
            Ok(r) => format!("Failed to delete collection: {}", error_detail(r).await),
            Err(e) => format!("Failed to delete collection: {e}"),
        }
    }

    #[tool(
        name = "update_collection",
        description = "Update a collection's name, description, or default citation style.",
        annotations(title = "Update collection metadata", read_only_hint = false, destructive_hint = false, idempotent_hint = true)
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
            _ => "Failed to update collection.".into(),
        }
    }

    #[tool(
        name = "remove_from_collection",
        description = "Remove a specific entry from a collection. Pass entry_id from search_collection, or a bare DOI / doi:10.x/y alias (resolved locally before the API call).",
        annotations(title = "Remove collection entry", read_only_hint = false, destructive_hint = true, idempotent_hint = false)
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
                    return format!(
                        "Removed entry {resolved_eid} from '{}'.",
                        args.collection
                    );
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
            Ok(r) => format!("Failed to remove entry: {}", error_detail(r).await),
            Err(e) => format!("Failed to remove entry: {e}"),
        }
    }

    #[tool(
        name = "update_tags",
        description = "Set tags on a collection. Replaces all existing tags.",
        annotations(title = "Update collection tags", read_only_hint = false, destructive_hint = false, idempotent_hint = true)
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
            _ => "Failed to update tags.".into(),
        }
    }

    #[tool(
        name = "reorder_collection",
        description = "Reorder entries in a collection. Provide the entry IDs in the desired order.",
        annotations(title = "Reorder collection entries", read_only_hint = false, destructive_hint = false, idempotent_hint = true)
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
            _ => "Failed to reorder collection.".into(),
        }
    }

    // --- Phase 3: Sharing and bulk ops ---

    #[tool(
        name = "share_collection",
        description = "Create a shareable link for a collection. Anyone with the link can view it. Requires academic/business plan.",
        annotations(title = "Share collection", read_only_hint = false, destructive_hint = false, idempotent_hint = true)
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
            _ => "Failed to share collection.".into(),
        }
    }

    #[tool(
        name = "unshare_collection",
        description = "Revoke the shareable link for a collection.",
        annotations(title = "Unshare collection", read_only_hint = false, destructive_hint = true, idempotent_hint = true)
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
            _ => "Failed to unshare collection.".into(),
        }
    }

    #[tool(
        name = "merge_collections",
        description = "Merge multiple collections into one. All entries are combined, duplicates are skipped.",
        annotations(title = "Merge collections", read_only_hint = false, destructive_hint = false, idempotent_hint = false)
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
            match self.request(endpoints::COLLECTIONS_LIST, &[]).send().await {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                _ => return "Failed to list collections.".into(),
            };

        let mut resolved = Vec::new();
        for name in &args.collections {
            let Some(col) = cols.iter().find(|c| c["name"].as_str() == Some(name)) else {
                return format!("Collection '{name}' not found.");
            };
            // Fetch full collection with entries
            let id = col["id"].as_str().unwrap_or("");
            let r = self
                .request(endpoints::COLLECTION_GET, &[("id", id)])
                .send()
                .await;
            match r {
                Ok(r) if r.status().is_success() => {
                    let full: serde_json::Value = r.json().await.unwrap_or_default();
                    resolved.push(full);
                }
                _ => return format!("Failed to load collection '{name}'."),
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
            Ok(r) => format!("Merge failed: {}", error_detail(r).await),
            Err(e) => format!("Merge failed: {e}"),
        }
    }

    #[tool(
        name = "batch_move_entries",
        description = "Move entries from one collection to another.",
        annotations(title = "Batch move entries", read_only_hint = false, destructive_hint = false, idempotent_hint = false)
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
            Ok(r) => format!("Batch move failed: {}", error_detail(r).await),
            Err(e) => format!("Batch move failed: {e}"),
        }
    }

    #[tool(
        name = "view_shared",
        description = "View a shared collection using its share token.",
        annotations(title = "View shared collection", read_only_hint = true, idempotent_hint = true)
    )]
    async fn view_shared(&self, Parameters(args): Parameters<ViewSharedArgs>) -> String {
        let r = self
            .request(endpoints::SHARED_GET, &[("token", &args.share_token)])
            .send()
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
            _ => "Failed to load shared collection.".into(),
        }
    }

    // --- Utility tools ---

    #[tool(
        name = "generate_citation_keys",
        description = "Generate Better BibTeX-style citation keys (e.g. 'goswami2026') for a list of DOIs. Requires academic/business plan.",
        annotations(title = "Generate citation keys", read_only_hint = true, idempotent_hint = true)
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
            .request(endpoints::CITATION_KEYS, &[])
            .json(&serde_json::json!({"entries": entries}))
            .send()
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
            Ok(r) => format!("Citation key generation failed: {}", error_detail(r).await),
            Err(e) => format!("Citation key generation failed: {e}"),
        }
    }

    #[tool(
        name = "expand_journal",
        description = "Expand a journal abbreviation to its full name (e.g. 'JACS' -> 'Journal of the American Chemical Society'). 16,000+ journals supported. Requires academic/business plan.",
        annotations(title = "Expand journal abbreviation", read_only_hint = true, idempotent_hint = true)
    )]
    async fn expand_journal(&self, Parameters(args): Parameters<ExpandJournalArgs>) -> String {
        let r = self
            .request(endpoints::JOURNAL_EXPAND, &[])
            .json(&serde_json::json!({"abbreviation": args.abbreviation}))
            .send()
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
            Ok(r) => format!("Journal expansion failed: {}", error_detail(r).await),
            Err(e) => format!("Journal expansion failed: {e}"),
        }
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        let mut caps = ServerCapabilities::default();
        caps.tools = Some(ToolsCapability { list_changed: None });
        let mut info = ServerInfo::new(caps);
        info.server_info.name = "ookcite-mcp".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(
            "OokCite provides citation METADATA validation and formatting -- it does NOT fetch PDFs, \
             full-text articles, or paper content. It returns structured metadata (title, authors, \
             year, journal, DOI) and formatted bibliography entries. \
             ALWAYS use these tools instead of searching the web for DOI or citation metadata. \
             PERFORMANCE: prefer batch tools over repeated single calls -- verify_references for many DOIs, \
             batch_format for many messy citations, batch_add_to_collection for many collection inserts, \
             import_bibliography for whole .bib/.ris files. Destructive tools (delete_collection, \
             remove_from_collection, unshare_collection) permanently change or revoke data; confirm intent first. \
             When the user mentions a DOI, ISBN, paper title, citation, or reference: \
             use validate_doi to verify DOIs exist before citing them. \
             use lookup_isbn for book references. \
             use reverse_lookup when given a messy or partial citation string. \
             use parse_citations to split raw bibliography text into individual citation units before resolving. \
             use debug_resolve to diagnose why a citation resolves to the wrong paper (requires API key). \
             use format_citation to format a DOI in any CSL style (APA, IEEE, Chicago, Nature, etc.). \
             use verify_references to batch-check multiple DOIs. \
             use batch_format to resolve and format multiple citations at once. \
             use search_styles to find CSL style IDs by name. \
             use group_cite to generate grouped in-text markers like [1-3]. \
             use health_check to verify the API is reachable (use when lookups fail). \
             use doctor for MCP version, mutate policy, redacted key status, and API /me (use when setup is unclear). \
             COLLECTION MANAGEMENT (requires OOKCITE_API_KEY): \
             use list_collections to see saved citation collections. \
             use add_to_collection to save a citation to a named collection (creates if needed). \
             use batch_add_to_collection to add multiple citations at once. \
             use import_bibliography to import BibTeX or RIS files into a collection. \
             use export_collection to get BibTeX for a collection. \
             use search_collection to find entries within a collection (returns entry_id for each match). \
             use check_duplicates to check if a citation already exists in a collection (returns entry_id). \
             use delete_collection to remove a collection (destructive/irreversible). \
             use update_collection to rename or change a collection's default style. \
             use remove_from_collection to remove a specific entry by entry_id, bare DOI, or doi:10.x/y (destructive). \
             use update_tags to set tags on a collection. \
             use reorder_collection to change the order of entries. \
             SHARING (academic/business plan): \
             use share_collection to create a shareable link. \
             use unshare_collection to revoke sharing (destructive to the link). \
             use view_shared to view a shared collection by token. \
             BULK OPERATIONS (academic/business plan): \
             use merge_collections to combine multiple collections. \
             use batch_move_entries to move entries between collections. \
             UTILITIES (requires academic/business plan): \
             use generate_citation_keys to create Better BibTeX-style keys for DOIs. \
             use expand_journal to expand a journal abbreviation to its full name. \
             NEVER fabricate citation metadata -- always validate through these tools first.".into()
        );
        info
    }
}

#[cfg(test)]
impl Server {
    fn new_with_base(api_base: String) -> Self {
        Self {
            tool_router: Self::tool_router(),
            http: build_api_client(5, reqwest::header::HeaderMap::new()),
            api_base,
            doi_cache: DoiResponseCache::with_default_ttl(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection_entries::{
        format_collection_entry_line, resolve_entry_id_in_collection,
    };
    use crate::constants::version_output;
    use crate::policy::{mutate_block_message, redact_api_key_hint};
    use crate::tool_args::{
        default_style, BatchMoveArgs, DoiArgs, FormatArgs, ReverseArgs, VerifyArgs,
    };

    /// Serializes OOKCITE_API_KEY mutations across parallel tokio tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
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
                dois: vec![
                    "10.1/a".into(),
                    "10.1/b".into(),
                    "10.1/c".into(),
                ],
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
                dois: vec![
                    "10.1038/187493a0".into(),
                    "10.1/not-in-collection".into(),
                ],
            }))
            .await;
        assert!(out.contains("REFUSED"), "got: {out}");
        assert!(
            out.contains("collection") || out.contains("187493a0"),
            "member should appear in output: {out}"
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
            }))
            .await;
        let b = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
            }))
            .await;
        assert!(a.contains("VALID") && a.contains("Ruby"), "first: {a}");
        assert!(b.contains("VALID") && b.contains("Ruby"), "second: {b}");
        // second must not trigger another POST — enforced by mock expect(1)
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
                citations: vec![
                    "10.1038/187493a0".into(),
                    "10.1/other".into(),
                ],
                style: default_style(),
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
        assert!(instr.contains("PERFORMANCE: prefer batch tools"));
        assert!(instr.contains("delete_collection"));
        assert!(instr.contains("destructive"));
        assert!(instr.contains("METADATA"));
        assert_eq!(info.server_info.name, "ookcite-mcp");
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
        assert_eq!(args.format, "bibtex");
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
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_server(base: &str) -> Server {
        Server::new_with_base(base.to_string())
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
            }))
            .await;
        assert!(result.starts_with("VALID"));
        assert!(result.contains("Stimulated Optical Radiation in Ruby"));
        assert!(result.contains("Maiman"));
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
            }))
            .await;
        assert!(result.starts_with("INVALID"));
        assert!(!result.contains("CrossRef"));
    }

    #[tokio::test]
    async fn test_validate_doi_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("Daily limit reached (50/day). Resets in 3h 45m."),
            )
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
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
                "message": "This feature requires an academic ($4/mo) or business ($12/mo) plan.",
                "upgrade_url": "https://my.turtletech.us"
            })))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
            }))
            .await;
        assert!(result.starts_with("ACCESS DENIED"));
        assert!(result.contains("academic"));
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
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/187493a0".into()],
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
            .respond_with(ResponseTemplate::new(503).set_body_string(
                "Lookup service temporarily unavailable. Please try again shortly.",
            ))
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .validate_doi(Parameters(DoiArgs {
                doi: "10.1038/187493a0".into(),
            }))
            .await;
        assert!(result.starts_with("TEMPORARY ERROR"));
        assert!(!result.contains("INVALID"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_success() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": [
                    {
                        "metadata": {
                            "title": "Stimulated Optical Radiation in Ruby",
                            "doi": "10.1038/187493a0",
                            "journal": "Nature"
                        },
                        "score": 95.0
                    }
                ]
            })))
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
    }

    #[tokio::test]
    async fn test_reverse_lookup_no_matches() {
        let mock = MockServer::start().await;
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
        assert_eq!(result, "No matches found");
    }

    #[tokio::test]
    async fn test_reverse_lookup_falls_back_to_live_when_local_empty() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":false"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "candidates": []
            })))
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
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":false"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": [
                    {
                        "metadata": {
                            "title": "Resource allocation based on redundancy models for high availability cloud",
                            "doi": "10.1007/s00607-019-00728-1",
                            "journal": "Computing"
                        },
                        "score": 2.0
                    }
                ]
            })))
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
    }

    #[tokio::test]
    async fn test_reverse_lookup_rejects_unconfident_local_and_live_matches() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains(r#""use_live_queries":false"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_type": "text",
                "match_type": "candidate_list",
                "candidates": [
                    {
                        "metadata": {
                            "title": "Resource allocation based on redundancy models for high availability cloud",
                            "doi": "10.1007/s00607-019-00728-1",
                            "journal": "Computing"
                        },
                        "score": 2.0
                    }
                ]
            })))
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
                            "doi": "10.5040/9781350101272.00000005",
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

        assert_eq!(result, "No confident matches found");
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
            .and(path("/api/v1/collections/col-123/entries/doi%3A10.1038%2Fnature14539"))
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
            }))
            .await;
        assert_eq!(result.lines().count(), 3);
        assert!(result.lines().all(|l| l.starts_with("VALID")));
    }

    #[tokio::test]
    async fn test_verify_references_retries_transient_lookup_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains("10.1038/retry"))
            .respond_with(
                ResponseTemplate::new(503)
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
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("Daily limit reached (30/day). Resets in 5h."),
            )
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/187493a0".into()],
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED 10.1038/187493a0 :"));
        assert!(!result.contains("INVALID"));
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
                    .set_body_string("Lookup service temporarily unavailable."),
            )
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .verify_references(Parameters(VerifyArgs {
                dois: vec!["10.1038/good".into(), "10.1038/slow".into()],
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
            .expect(0)
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let metadata = s
            .resolve_query_to_metadata("Wright 1931 genetics shifting balance", false)
            .await
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
            .expect(0)
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
                "message": "This feature requires an academic ($4/mo) or business ($12/mo) plan."
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
        assert!(detail.contains("academic"));
        assert!(detail.contains("$4/mo"));
    }

    #[tokio::test]
    async fn test_format_citation_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("Daily limit reached (30/day). Resets in 5h."),
            )
            .mount(&mock)
            .await;

        let s = test_server(&mock.uri());
        let result = s
            .format_citation(Parameters(FormatArgs {
                doi: "10.1038/187493a0".into(),
                style: "apa".into(),
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED"));
        assert!(!result.contains("not found"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
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
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(503).set_body_string(
                "Lookup service temporarily unavailable. Please try again shortly.",
            ))
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
}
