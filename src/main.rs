//! # OokCite MCP Server
//!
//! A [Model Context Protocol](https://modelcontextprotocol.io/) server that
//! gives LLMs the ability to validate DOIs, format citations, and catch
//! hallucinated academic references.
//!
//! ## Tools
//!
//! **Lookup & Validation**
//! * **validate_doi** : Check if a DOI exists (anti-hallucination)
//! * **lookup_isbn** : Look up a book by ISBN
//! * **reverse_lookup** : Find a paper from messy citation text
//! * **parse_citations** : Parse raw bibliography text into structured units
//! * **debug_resolve** : Debug why a citation resolves incorrectly
//! * **health_check** : Check API availability and health
//!
//! **Formatting**
//! * **format_citation** : Format a DOI in any of 2900+ CSL styles
//! * **verify_references** : Batch-check a list of DOIs
//! * **batch_format** : Resolve and format multiple citations at once
//! * **search_styles** : Find CSL style IDs by name
//! * **group_cite** : Generate grouped in-text markers
//!
//! **Collections**
//! * **list_collections** : List user's citation collections
//! * **add_to_collection** : Add a single citation to a collection
//! * **batch_add_to_collection** : Add multiple citations at once
//! * **import_bibliography** : Import BibTeX/RIS files into a collection
//! * **export_collection** : Export collection as BibTeX
//! * **search_collection** : Search within a collection
//! * **check_duplicates** : Check for duplicate entries
//! * **delete_collection** : Delete a collection
//! * **update_collection** : Update collection metadata
//! * **remove_from_collection** : Remove a specific entry
//! * **update_tags** : Set tags on a collection
//! * **reorder_collection** : Reorder entries
//!
//! **Sharing**
//! * **share_collection** : Create a shareable link
//! * **unshare_collection** : Revoke sharing
//! * **view_shared** : View a shared collection
//! * **merge_collections** : Merge multiple collections
//! * **batch_move_entries** : Move entries between collections
//!
//! ## Usage
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "ookcite": {
//!       "command": "ookcite-mcp",
//!       "env": {
//!         "OOKCITE_API_KEY": "your_api_key_here"
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! Connects to the public OokCite API at <https://ookcite.turtletech.us>.
//! Basic usage requires no API key, but adding one unlocks higher rate limits.

mod setup;

use rmcp::ServerHandler;
use rmcp::{
    handler::server::{
        tool::ToolRouter,
        wrapper::Parameters,
    },
    model::*,
    tool, tool_handler, tool_router, ServiceExt,
};
use futures::{stream, StreamExt};
use serde::Deserialize;
use ookcite_mcp::endpoints::{self, Endpoint};

const API: &str = "https://ookcite-api.turtletech.us";

#[derive(Clone)]
struct Server {
    tool_router: ToolRouter<Self>,
    http: reqwest::Client,
    api_base: String,
}

/// Extract a useful error message from a failed HTTP response.
async fn error_detail(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    // Try to parse JSON error with message field
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(msg) = json["message"].as_str() {
            return format!("{status}: {msg}");
        }
    }
    if body.len() > 120 {
        format!("{status}: {}", &body[..120])
    } else if body.is_empty() {
        format!("{status}")
    } else {
        format!("{status}: {body}")
    }
}

async fn classify_lookup_doi_failure(resp: reqwest::Response, doi: &str) -> String {
    if resp.status().as_u16() == 429 {
        format!("RATE LIMITED {doi} : {}", error_detail(resp).await)
    } else if matches!(resp.status().as_u16(), 401 | 403) {
        format!("ACCESS DENIED {doi} : {}", error_detail(resp).await)
    } else if resp.status().is_server_error() {
        format!("TEMPORARY ERROR {doi} : {}", error_detail(resp).await)
    } else if resp.status().is_client_error() && resp.status().as_u16() != 404 {
        format!("CLIENT ERROR {doi} : {}", error_detail(resp).await)
    } else {
        format!("INVALID {doi} : HTTP {}", resp.status())
    }
}

// Args

#[derive(Deserialize, schemars::JsonSchema)]
struct DoiArgs {
    /// DOI to validate (e.g. "10.1038/187493a0")
    doi: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct IsbnArgs {
    /// ISBN to look up (e.g. "978-0-521-85629-7")
    isbn: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReverseArgs {
    /// Free-text search: DOI, ISBN, author name, title, journal, or any combination (e.g. "Goswami JCTC 2026", "Einstein relativity 1905")
    text: String,
    /// Optional author name to filter/boost results (e.g. "Goswami")
    #[serde(default)]
    author: Option<String>,
    /// Optional journal name to filter/boost results (e.g. "MethodsX", "JCTC")
    #[serde(default)]
    journal: Option<String>,
    /// Optional publication year to filter/boost results (e.g. 2026)
    #[serde(default)]
    year: Option<i32>,
    /// Optional ORCID ID to filter/boost results by author's ORCID (e.g. "0000-0002-1234-5678")
    #[serde(default)]
    orcid: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ParseCitationsArgs {
    /// Raw bibliography text (can contain multiple citations separated by newlines or numbered)
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DebugResolveArgs {
    /// Citation text to debug (free-text query)
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct FormatArgs {
    /// DOI of the paper
    doi: String,
    /// CSL style (default: "apa"). Options: apa, ieee, chicago-author-date, mla, nature, vancouver, etc.
    #[serde(default = "default_style")]
    style: String,
}
fn default_style() -> String {
    "apa".into()
}

#[derive(Deserialize, schemars::JsonSchema)]
struct VerifyArgs {
    /// List of DOIs to verify exist
    dois: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct BatchArgs {
    /// Citation strings to resolve (one per entry)
    citations: Vec<String>,
    /// CSL style for formatting
    #[serde(default = "default_style")]
    style: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct StyleSearchArgs {
    /// Query to search for a CSL style (e.g., "american chemical society", "apa", "ieee")
    query: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GroupCiteArgs {
    /// List of DOIs to group into a single in-text citation
    dois: Vec<String>,
    /// CSL style (default: "apa")
    #[serde(default = "default_style")]
    style: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListCollectionsArgs {}

#[derive(Deserialize, schemars::JsonSchema)]
struct AddToCollectionArgs {
    /// Collection name (creates if doesn't exist)
    collection: String,
    /// DOI, ISBN, or free-text search to find the paper to add
    query: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ExportCollectionArgs {
    /// Collection name to export
    collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchCollectionArgs {
    /// Collection name to search within
    collection: String,
    /// Search query (matches title, author, journal)
    query: String,
}

// --- Phase 1: High-value new tools ---

#[derive(Deserialize, schemars::JsonSchema)]
struct HealthCheckArgs {}

#[derive(Deserialize, schemars::JsonSchema)]
struct ImportBibliographyArgs {
    /// Collection name (creates if doesn't exist)
    collection: String,
    /// BibTeX or RIS content to import
    content: String,
    /// File format: "bibtex" or "ris" (default: "bibtex")
    #[serde(default = "default_bibtex")]
    format: String,
}
fn default_bibtex() -> String {
    "bibtex".into()
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CheckDuplicatesArgs {
    /// Collection name to check within
    collection: String,
    /// DOI or free-text query to check for duplicates
    query: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct BatchAddArgs {
    /// Collection name (creates if doesn't exist)
    collection: String,
    /// List of DOIs or free-text queries to resolve and add
    queries: Vec<String>,
}

// --- Phase 2: Collection management ---

#[derive(Deserialize, schemars::JsonSchema)]
struct DeleteCollectionArgs {
    /// Collection name to delete
    collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateCollectionArgs {
    /// Current collection name
    collection: String,
    /// New name (optional)
    #[serde(default)]
    name: Option<String>,
    /// New description (optional)
    #[serde(default)]
    description: Option<String>,
    /// New default CSL style (optional)
    #[serde(default)]
    default_style: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RemoveFromCollectionArgs {
    /// Collection name
    collection: String,
    /// Entry ID to remove (from search_collection or list)
    entry_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateTagsArgs {
    /// Collection name
    collection: String,
    /// New tag list (replaces existing tags)
    tags: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReorderCollectionArgs {
    /// Collection name
    collection: String,
    /// Ordered list of entry IDs
    entry_ids: Vec<String>,
}

// --- Phase 3: Sharing and bulk ops ---

#[derive(Deserialize, schemars::JsonSchema)]
struct ShareCollectionArgs {
    /// Collection name to share
    collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UnshareCollectionArgs {
    /// Collection name to unshare
    collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MergeCollectionsArgs {
    /// Collection names to merge (first becomes target)
    collections: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct BatchMoveArgs {
    /// Source collection name
    source: String,
    /// Target collection name
    target: String,
    /// Entry IDs to move
    entry_ids: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ViewSharedArgs {
    /// Share token from a shared collection URL
    share_token: String,
}

// --- New utility tools ---

#[derive(Deserialize, schemars::JsonSchema)]
struct GenerateCitationKeysArgs {
    /// List of DOIs to generate citation keys for
    dois: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ExpandJournalArgs {
    /// Journal abbreviation to expand (e.g. "JACS", "J. Am. Chem. Soc.")
    abbreviation: String,
}

// Tools

#[tool_router]
impl Server {
    fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();

        if let Ok(api_key) = std::env::var("OOKCITE_API_KEY") {
            if let Ok(mut auth_val) =
                format!("Bearer {api_key}").parse::<reqwest::header::HeaderValue>()
            {
                auth_val.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, auth_val);
            }
        } else {
            eprintln!(
                "ookcite-mcp: OOKCITE_API_KEY not set; requests will be anonymous/IP-rate-limited"
            );
        }

        Self {
            tool_router: Self::tool_router(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .default_headers(headers)
                .build()
                .unwrap(),
            api_base: API.to_string(),
        }
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
            other => panic!("ookcite-mcp: unsupported HTTP method `{other}` in registry for {}", ep.path),
        }
    }

    #[tool(
        name = "search_styles",
        description = "Search for available CSL citation styles by name. Returns a list of matching style IDs to use in formatting tools."
    )]
    async fn search_styles(
        &self,
        Parameters(args): Parameters<StyleSearchArgs>,
    ) -> String {
        let r = self.request(endpoints::STYLES_SEARCH, &[])
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
                if out.is_empty() { "No styles found".into() } else { out.join("\n") }
            }
            _ => "Style search failed".into(),
        }
    }

    #[tool(
        name = "validate_doi",
        description = "Check if a DOI exists and return its metadata. Use this to verify citations are real. Returns title, authors, year, journal, volume, and issue."
    )]
    async fn validate_doi(&self, Parameters(args): Parameters<DoiArgs>) -> String {
        let r = self.request(endpoints::LOOKUP_DOI, &[])
            .json(&serde_json::json!({"doi": args.doi}))
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
                let journal = meta["journal"].as_str().unwrap_or("N/A");
                let volume = meta["volume"].as_str().unwrap_or("N/A");
                let issue = meta["issue"].as_str().unwrap_or("N/A");
                let doi = meta["doi"].as_str().unwrap_or(&args.doi);
                format!("VALID\nDOI: {doi}\nTitle: {title}\nAuthors: {authors}\nYear: {year}\nJournal: {journal}\nVolume: {volume}\nIssue: {issue}")
            }
            Ok(resp) if resp.status().as_u16() == 429 => {
                format!("RATE LIMITED: {}", error_detail(resp).await)
            }
            Ok(resp) if matches!(resp.status().as_u16(), 401 | 403) => {
                format!("ACCESS DENIED: {}", error_detail(resp).await)
            }
            Ok(resp) if resp.status().is_server_error() => {
                format!("TEMPORARY ERROR: {}", error_detail(resp).await)
            }
            Ok(resp) if resp.status().is_client_error() && resp.status().as_u16() != 404 => {
                format!("CLIENT ERROR: {}", error_detail(resp).await)
            }
            Ok(_) => format!(
                "INVALID: DOI {} not found. This citation may be a hallucination.",
                args.doi
            ),
            Err(e) => format!("ERROR: Could not reach citation service: {e}"),
        }
    }

    #[tool(
        name = "lookup_isbn",
        description = "Look up a book by ISBN. Returns title, authors, publisher, year, and pages."
    )]
    async fn lookup_isbn(&self, Parameters(args): Parameters<IsbnArgs>) -> String {
        let r = self.request(endpoints::LOOKUP_ISBN, &[])
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
            Ok(r) if r.status().as_u16() == 429 => format!("RATE LIMITED: {}", error_detail(r).await),
            Ok(r) if r.status().as_u16() == 403 => format!("ACCESS DENIED: {}", error_detail(r).await),
            Ok(r) if r.status().is_server_error() => format!("TEMPORARY ERROR: {}", error_detail(r).await),
            Ok(_) => format!("ISBN {} not found", args.isbn),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    #[tool(
        name = "reverse_lookup",
        description = "Parse a messy citation string and find the matching paper. Returns ranked candidates. Optional filters (author, journal, year, orcid) boost matching results."
    )]
    async fn reverse_lookup(&self, Parameters(args): Parameters<ReverseArgs>) -> String {
        let mut body = serde_json::json!({"text": args.text});
        // Build filters object for optional search cues
        let mut filters = serde_json::Map::new();
        if let Some(author) = &args.author {
            filters.insert("author".into(), serde_json::json!(author));
        }
        if let Some(journal) = &args.journal {
            filters.insert("journal".into(), serde_json::json!(journal));
        }
        if let Some(year) = args.year {
            filters.insert("year".into(), serde_json::json!(year));
        }
        if let Some(orcid) = &args.orcid {
            filters.insert("orcid".into(), serde_json::json!(orcid));
        }
        if !filters.is_empty() {
            body["filters"] = serde_json::Value::Object(filters);
        }
        let r = self.request(endpoints::REVERSE, &[])
            .json(&body)
            .send()
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let candidates: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                let mut out = Vec::new();
                for (i, c) in candidates.iter().enumerate() {
                    let title = c["metadata"]["title"].as_str().unwrap_or("?");
                    let doi = c["metadata"]["doi"].as_str().unwrap_or("?");
                    let journal = c["metadata"]["journal"].as_str().unwrap_or("N/A");
                    let score = c["score"].as_f64().unwrap_or(0.0);
                    out.push(format!(
                        "{}. [score:{:.0}] {title} | {journal} (doi:{doi})",
                        i + 1,
                        score
                    ));
                }
                if out.is_empty() { "No matches found".into() } else { out.join("\n") }
            }
            Ok(r) if r.status().as_u16() == 429 => format!("RATE LIMITED: {}", error_detail(r).await),
            Ok(r) if r.status().as_u16() == 403 => format!("ACCESS DENIED: {}", error_detail(r).await),
            Ok(r) if r.status().is_server_error() => format!("TEMPORARY ERROR: {}", error_detail(r).await),
            Ok(_) => "No matches found".into(),
            Err(e) => format!("Reverse lookup failed: {e}"),
        }
    }

    #[tool(
        name = "parse_citations",
        description = "Parse raw bibliography text into structured citation units. Splits multi-citation blocks, extracts DOIs/ISBNs, and provides title/author/year hints. Use this to break down pasted bibliographies before resolving individual citations."
    )]
    async fn parse_citations(&self, Parameters(args): Parameters<ParseCitationsArgs>) -> String {
        let r = self.request(endpoints::PARSE_CITATIONS, &[])
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
                            if let Some(t) = title { hints.push(format!("title: {t}")); }
                            if let Some(a) = author { hints.push(format!("author: {a}")); }
                            if let Some(y) = year { hints.push(format!("year: {y}")); }
                            if !hints.is_empty() {
                                entry.push_str(&format!("\n   Hints: {} (parser: {})", hints.join(", "), parser));
                            }
                            out.push(entry);
                        }
                        format!("Found {} citations:\n\n{}", arr.len(), out.join("\n\n"))
                    }
                    None => "No citations found in text".into(),
                }
            }
            Ok(r) if r.status().as_u16() == 429 => format!("RATE LIMITED: {}", error_detail(r).await),
            Ok(r) if r.status().as_u16() == 403 => format!("ACCESS DENIED: {}", error_detail(r).await),
            Ok(r) if r.status().is_server_error() => format!("TEMPORARY ERROR: {}", error_detail(r).await),
            Ok(_) => "Failed to parse citations".into(),
            Err(e) => format!("Parse citations failed: {e}"),
        }
    }

    #[tool(
        name = "debug_resolve",
        description = "Debug why a citation resolves incorrectly. Returns the search query used, active ranking weights, and per-backend candidate lists with scores. Use this to diagnose bad matches."
    )]
    async fn debug_resolve(&self, Parameters(args): Parameters<DebugResolveArgs>) -> String {
        let r = self.request(endpoints::RESOLVE_DEBUG, &[])
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
            Ok(r) if r.status().as_u16() == 401 => "AUTH REQUIRED: debug_resolve requires authentication (API key)".into(),
            Ok(r) if r.status().as_u16() == 429 => format!("RATE LIMITED: {}", error_detail(r).await),
            Ok(r) if r.status().as_u16() == 403 => format!("ACCESS DENIED: {}", error_detail(r).await),
            Ok(r) if r.status().is_server_error() => format!("TEMPORARY ERROR: {}", error_detail(r).await),
            Ok(_) => "Debug resolve failed".into(),
            Err(e) => format!("Debug resolve failed: {e}"),
        }
    }

    #[tool(
        name = "format_citation",
        description = "Format a citation by DOI in a specific CSL style. Returns both the in-text marker and the full bibliography entry."
    )]
    async fn format_citation(&self, Parameters(args): Parameters<FormatArgs>) -> String {
        let lookup = self.request(endpoints::LOOKUP_DOI, &[])
            .json(&serde_json::json!({"doi": args.doi}))
            .send()
            .await;
        let meta: serde_json::Value = match lookup {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            Ok(r) if r.status().as_u16() == 429 => return format!("RATE LIMITED: {}", error_detail(r).await),
            Ok(r) if r.status().as_u16() == 403 => return format!("ACCESS DENIED: {}", error_detail(r).await),
            Ok(_) => return format!("DOI {} not found", args.doi),
            Err(e) => return format!("ERROR: {e}"),
        };

        let fmt = self.request(endpoints::FORMAT, &[])
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
        description = "Generate a grouped in-text citation marker (e.g., '[1-3]') for multiple DOIs."
    )]
    async fn group_cite(&self, Parameters(args): Parameters<GroupCiteArgs>) -> String {
        let api_base = self.api_base.clone();
        let futs: Vec<_> = args.dois.iter().map(|doi| {
            let http = self.http.clone();
            let api_base = api_base.clone();
            let doi = doi.clone();
            async move {
                let r = http
                    .post(endpoints::LOOKUP_DOI.url(&api_base, &[]))
                    .json(&serde_json::json!({"doi": doi}))
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status().is_success() => resp.json::<serde_json::Value>().await.ok(),
                    _ => None,
                }
            }
        }).collect();
        let entries: Vec<serde_json::Value> = stream::iter(futs)
            .buffer_unordered(10)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();

        if entries.is_empty() {
            return "Failed to resolve any DOIs.".into();
        }

        let indices: Vec<usize> = (0..entries.len()).collect();
        let r = self.request(endpoints::FORMAT_GROUP_CITE, &[])
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
        description = "Batch verify that a list of DOIs exist. Returns VALID or INVALID for each."
    )]
    async fn verify_references(
        &self,
        Parameters(args): Parameters<VerifyArgs>,
    ) -> String {
        let api_base = self.api_base.clone();
        let futs: Vec<_> = args.dois.iter().map(|doi| {
            let http = self.http.clone();
            let api_base = api_base.clone();
            let doi = doi.clone();
            async move {
                let r = http
                    .post(endpoints::LOOKUP_DOI.url(&api_base, &[]))
                    .json(&serde_json::json!({"doi": doi}))
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status().is_success() => {
                        let meta: serde_json::Value = resp.json().await.unwrap_or_default();
                        let title = meta["title"].as_str().unwrap_or("?");
                        format!("VALID {doi} : {title}")
                    }
                    Ok(resp) => classify_lookup_doi_failure(resp, &doi).await,
                    Err(e) => format!("ERROR {doi} : {e}"),
                }
            }
        }).collect();
        let results = stream::iter(futs).buffer_unordered(10).collect::<Vec<_>>().await;
        results.join("\n")
    }

    #[tool(
        name = "batch_format",
        description = "Resolve and format multiple messy citations at once. Pass citation strings in any format."
    )]
    async fn batch_format(&self, Parameters(args): Parameters<BatchArgs>) -> String {
        // Resolve all citations in parallel (up to 10 concurrent)
        let api_base = self.api_base.clone();
        let futs: Vec<_> = args.citations.iter().enumerate().map(|(i, text)| {
            let http = self.http.clone();
            let api_base = api_base.clone();
            let text = text.clone();
            async move {
                let r = http
                    .post(endpoints::REVERSE.url(&api_base, &[]))
                    .json(&serde_json::json!({"text": text}))
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status().is_success() => {
                        let candidates: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                        if let Some(meta) = candidates.first().and_then(|c| c.get("metadata")) {
                            Ok(meta.clone())
                        } else {
                            Err(format!("[{}] Not found: {}", i + 1, &text[..text.len().min(60)]))
                        }
                    }
                    Ok(resp) => Err(format!("[{}] HTTP {}: {}", i + 1, resp.status(), &text[..text.len().min(60)])),
                    Err(e) => Err(format!("[{}] {e}: {}", i + 1, &text[..text.len().min(60)])),
                }
            }
        }).collect();
        let resolved: Vec<_> = stream::iter(futs).buffer_unordered(10).collect().await;

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
        let fmt = self.request(endpoints::FORMAT, &[])
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
        description = "List all citation collections for the authenticated user. Requires OOKCITE_API_KEY."
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
                                if t.is_empty() { String::new() }
                                else { format!(" [{}]", t.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")) }
                            })
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Ok(r) if r.status().as_u16() == 401 => "Authentication required. Set OOKCITE_API_KEY.".into(),
            Ok(r) if r.status().as_u16() == 503 => "Collections not available (S3 not configured).".into(),
            _ => "Failed to list collections.".into(),
        }
    }

    #[tool(
        name = "add_to_collection",
        description = "Add a citation to a collection. Searches by DOI, ISBN, or free-text (e.g. 'Goswami JCTC 2026'). Creates the collection if it doesn't exist."
    )]
    async fn add_to_collection(
        &self,
        Parameters(args): Parameters<AddToCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_or_create_collection(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let Some(metadata) = self.resolve_query_to_metadata(&args.query).await else {
            return format!("Could not resolve: {}", args.query);
        };

        let r = self.request(endpoints::COLLECTION_ENTRIES_ADD, &[("id", &col_id)])
            .json(&serde_json::json!({"metadata": metadata}))
            .send().await;
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
        description = "Export a collection as BibTeX. Returns the full .bib file content with Better BibTeX keys."
    )]
    async fn export_collection(
        &self,
        Parameters(args): Parameters<ExportCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let r = self.request(endpoints::COLLECTION_EXPORT_BIB, &[("id", &col_id)]).send().await;
        match r {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_else(|_| "Export failed.".into()),
            _ => "Failed to export collection.".into(),
        }
    }

    #[tool(
        name = "search_collection",
        description = "Search within a collection by author name, title keywords, or journal. Returns matching entries."
    )]
    async fn search_collection(
        &self,
        Parameters(args): Parameters<SearchCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let r = self.request(endpoints::COLLECTION_GET, &[("id", &col_id)]).send().await;
        let collection: serde_json::Value = match r {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => return "Failed to load collection.".into(),
        };

        let query_lower = args.query.to_lowercase();
        let entries = collection["entries"].as_array().cloned().unwrap_or_default();
        let matches: Vec<String> = entries.iter().filter(|e| {
            let meta = &e["metadata"];
            let title = meta["title"].as_str().unwrap_or("").to_lowercase();
            let authors = meta["authors"].as_array().map(|a| {
                a.iter().filter_map(|p| p["family"].as_str()).collect::<Vec<_>>().join(" ").to_lowercase()
            }).unwrap_or_default();
            let journal = meta["journal"].as_str().unwrap_or("").to_lowercase();
            title.contains(&query_lower) || authors.contains(&query_lower) || journal.contains(&query_lower)
        }).map(|e| {
            let meta = &e["metadata"];
            let title = meta["title"].as_str().unwrap_or("?");
            let authors = meta["authors"].as_array().map(|a| {
                a.iter().filter_map(|p| p["family"].as_str()).collect::<Vec<_>>().join(", ")
            }).unwrap_or_default();
            let year = meta["date"]["year"].as_i64().map(|y| format!(" ({y})")).unwrap_or_default();
            format!("- {authors}{year}: {title}")
        }).collect();

        if matches.is_empty() {
            format!("No entries matching '{}' in collection '{}'.", args.query, args.collection)
        } else {
            format!("{} matches in '{}':\n{}", matches.len(), args.collection, matches.join("\n"))
        }
    }

    // --- Helper: resolve collection name to ID ---

    async fn resolve_collection_id(&self, name: &str) -> Result<String, String> {
        let cols: Vec<serde_json::Value> = match self.request(endpoints::COLLECTIONS_LIST, &[]).send().await {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            Ok(r) if r.status().as_u16() == 401 => return Err("Authentication required. Set OOKCITE_API_KEY.".into()),
            _ => return Err("Failed to list collections.".into()),
        };
        cols.iter()
            .find(|c| c["name"].as_str() == Some(name))
            .and_then(|c| c["id"].as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("Collection '{name}' not found."))
    }

    async fn resolve_or_create_collection(&self, name: &str) -> Result<String, String> {
        match self.resolve_collection_id(name).await {
            Ok(id) => Ok(id),
            Err(_) => {
                let r = self.request(endpoints::COLLECTIONS_CREATE, &[])
                    .json(&serde_json::json!({"name": name}))
                    .send().await;
                match r {
                    Ok(r) if r.status().is_success() => {
                        let c: serde_json::Value = r.json().await.unwrap_or_default();
                        c["id"].as_str().map(|s| s.to_string())
                            .ok_or_else(|| {
                                format!(
                                    "Collection '{}' was created but the API response did not include an id.",
                                    name
                                )
                            })
                    }
                    Ok(r) => Err(format!(
                        "Failed to create collection '{}': {}",
                        name,
                        error_detail(r).await
                    )),
                    Err(e) => Err(format!("Failed to create collection '{}': {e}", name)),
                }
            }
        }
    }

    async fn resolve_query_to_metadata(&self, query: &str) -> Option<serde_json::Value> {
        let q = query.trim();
        if q.starts_with("10.") {
            let r = self.request(endpoints::LOOKUP_DOI, &[])
                .json(&serde_json::json!({"doi": q})).send().await;
            match r {
                Ok(r) if r.status().is_success() => Some(r.json::<serde_json::Value>().await.unwrap_or_default()),
                _ => None,
            }
        } else {
            let resolve = self
                .request(endpoints::RESOLVE, &[])
                .json(&serde_json::json!({
                    "input": { "kind": "Text", "text": q },
                    "filters": {},
                    "options": {}
                }))
                .send()
                .await;
            match resolve {
                Ok(r) if r.status().is_success() => {
                    let payload: serde_json::Value = r.json().await.unwrap_or_default();
                    if let Some(paper) = payload.get("paper").cloned() {
                        return Some(paper);
                    }
                }
                _ => {}
            }

            let reverse = self.request(endpoints::REVERSE, &[])
                .json(&serde_json::json!({"text": q})).send().await;
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
        description = "Check if the OokCite API is reachable and healthy. Returns service status and cache statistics."
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
            Ok(resp) => format!("API unhealthy: HTTP {}", resp.status()),
            Err(e) => format!("API unreachable: {e}"),
        }
    }

    #[tool(
        name = "import_bibliography",
        description = "Import a BibTeX (.bib) or RIS file into a collection. Pass the file content as a string. Creates the collection if it doesn't exist."
    )]
    async fn import_bibliography(
        &self,
        Parameters(args): Parameters<ImportBibliographyArgs>,
    ) -> String {
        let col_id = match self.resolve_or_create_collection(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let filename = if args.format == "ris" { "import.ris" } else { "import.bib" };
        let part = match reqwest::multipart::Part::text(args.content)
            .file_name(filename.to_string())
            .mime_str("text/plain")
        {
            Ok(p) => p,
            Err(_) => return "Failed to construct upload.".into(),
        };
        let form = reqwest::multipart::Form::new().part("file", part);

        let r = self.request(endpoints::COLLECTION_IMPORT, &[("id", &col_id)])
            .multipart(form)
            .send().await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let added = data["added"].as_u64().unwrap_or(0);
                let dupes = data["duplicates_skipped"].as_u64().unwrap_or(0);
                format!("Imported into '{}': {added} added, {dupes} duplicates skipped", args.collection)
            }
            Ok(r) if r.status().as_u16() == 401 => "Authentication required. Set OOKCITE_API_KEY.".into(),
            Ok(r) => format!("Import failed: {}", error_detail(r).await),
            Err(e) => format!("Import failed: {e}"),
        }
    }

    #[tool(
        name = "check_duplicates",
        description = "Check if a citation already exists in a collection. Resolves the query first, then checks for duplicates."
    )]
    async fn check_duplicates(
        &self,
        Parameters(args): Parameters<CheckDuplicatesArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let Some(metadata) = self.resolve_query_to_metadata(&args.query).await else {
            return format!("Could not resolve: {}", args.query);
        };

        let r = self.request(endpoints::COLLECTION_CHECK_DUPLICATES, &[("id", &col_id)])
            .json(&serde_json::json!({"metadata": metadata}))
            .send().await;
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
                        out.push(format!("- {match_type} ({similarity:.0}%) entry:{entry_id}"));
                    }
                    out.join("\n")
                }
            }
            _ => "Duplicate check failed.".into(),
        }
    }

    #[tool(
        name = "batch_add_to_collection",
        description = "Add multiple citations to a collection at once. Each query can be a DOI or free-text search."
    )]
    async fn batch_add_to_collection(
        &self,
        Parameters(args): Parameters<BatchAddArgs>,
    ) -> String {
        let col_id = match self.resolve_or_create_collection(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        // Resolve all queries in parallel (up to 10 concurrent)
        let futs: Vec<_> = args.queries.iter().enumerate().map(|(i, query)| {
            let server = self.clone();
            let query = query.clone();
            async move {
                let q = query.trim();
                let meta = server.resolve_query_to_metadata(q).await;
                match meta {
                    Some(m) => Ok(m),
                    None => Err(format!("[{}] Could not resolve: {}", i + 1, &query[..query.len().min(60)])),
                }
            }
        }).collect();
        let resolved: Vec<_> = stream::iter(futs).buffer_unordered(10).collect().await;

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

        let r = self.request(endpoints::COLLECTION_ENTRIES_BATCH, &[("id", &col_id)])
            .json(&serde_json::json!({"entries": entries}))
            .send().await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let added = data["added"].as_u64().unwrap_or(0);
                let dupes = data["duplicates_skipped"].as_u64().unwrap_or(0);
                let mut out = format!("Added {added} to '{}', {dupes} duplicates skipped", args.collection);
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
        description = "Delete a citation collection. This is irreversible."
    )]
    async fn delete_collection(
        &self,
        Parameters(args): Parameters<DeleteCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self.request(endpoints::COLLECTION_DELETE, &[("id", &col_id)]).send().await;
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
        description = "Update a collection's name, description, or default citation style."
    )]
    async fn update_collection(
        &self,
        Parameters(args): Parameters<UpdateCollectionArgs>,
    ) -> String {
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
        let r = self.request(endpoints::COLLECTION_UPDATE, &[("id", &col_id)])
            .json(&serde_json::Value::Object(body))
            .send().await;
        match r {
            Ok(r) if r.status().is_success() => {
                format!("Updated collection '{}'.", args.collection)
            }
            _ => "Failed to update collection.".into(),
        }
    }

    #[tool(
        name = "remove_from_collection",
        description = "Remove a specific entry from a collection by its entry ID."
    )]
    async fn remove_from_collection(
        &self,
        Parameters(args): Parameters<RemoveFromCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self.request(endpoints::COLLECTION_ENTRY_REMOVE, &[("id", &col_id), ("eid", &args.entry_id)])
            .send().await;
        match r {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {
                format!("Removed entry {} from '{}'.", args.entry_id, args.collection)
            }
            _ => "Failed to remove entry.".into(),
        }
    }

    #[tool(
        name = "update_tags",
        description = "Set tags on a collection. Replaces all existing tags."
    )]
    async fn update_tags(
        &self,
        Parameters(args): Parameters<UpdateTagsArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self.request(endpoints::COLLECTION_TAGS, &[("id", &col_id)])
            .json(&serde_json::json!({"tags": args.tags}))
            .send().await;
        match r {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {
                format!("Updated tags on '{}'.", args.collection)
            }
            _ => "Failed to update tags.".into(),
        }
    }

    #[tool(
        name = "reorder_collection",
        description = "Reorder entries in a collection. Provide the entry IDs in the desired order."
    )]
    async fn reorder_collection(
        &self,
        Parameters(args): Parameters<ReorderCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self.request(endpoints::COLLECTION_REORDER, &[("id", &col_id)])
            .json(&serde_json::json!({"entry_ids": args.entry_ids}))
            .send().await;
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
        description = "Create a shareable link for a collection. Anyone with the link can view it."
    )]
    async fn share_collection(
        &self,
        Parameters(args): Parameters<ShareCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self.request(endpoints::COLLECTION_SHARE, &[("id", &col_id)])
            .send().await;
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
        description = "Revoke the shareable link for a collection."
    )]
    async fn unshare_collection(
        &self,
        Parameters(args): Parameters<UnshareCollectionArgs>,
    ) -> String {
        let col_id = match self.resolve_collection_id(&args.collection).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self.request(endpoints::COLLECTION_UNSHARE, &[("id", &col_id)])
            .send().await;
        match r {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 204 => {
                format!("Unshared '{}'.", args.collection)
            }
            _ => "Failed to unshare collection.".into(),
        }
    }

    #[tool(
        name = "merge_collections",
        description = "Merge multiple collections into one. All entries are combined, duplicates are skipped."
    )]
    async fn merge_collections(
        &self,
        Parameters(args): Parameters<MergeCollectionsArgs>,
    ) -> String {
        if args.collections.len() < 2 {
            return "Need at least 2 collection names to merge.".into();
        }
        // Resolve all collections to full objects
        let cols: Vec<serde_json::Value> = match self.request(endpoints::COLLECTIONS_LIST, &[]).send().await {
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
            let r = self.request(endpoints::COLLECTION_GET, &[("id", id)]).send().await;
            match r {
                Ok(r) if r.status().is_success() => {
                    let full: serde_json::Value = r.json().await.unwrap_or_default();
                    resolved.push(full);
                }
                _ => return format!("Failed to load collection '{name}'."),
            }
        }

        let r = self.request(endpoints::COLLECTIONS_MERGE, &[])
            .json(&serde_json::json!({"collections": resolved}))
            .send().await;
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
        description = "Move entries from one collection to another."
    )]
    async fn batch_move_entries(
        &self,
        Parameters(args): Parameters<BatchMoveArgs>,
    ) -> String {
        let source_id = match self.resolve_collection_id(&args.source).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let target_id = match self.resolve_collection_id(&args.target).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let r = self.request(endpoints::COLLECTIONS_BATCH_MOVE, &[])
            .json(&serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "entry_ids": args.entry_ids
            }))
            .send().await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let moved = data["moved"].as_u64().unwrap_or(0);
                format!("Moved {moved} entries from '{}' to '{}'.", args.source, args.target)
            }
            Ok(r) => format!("Batch move failed: {}", error_detail(r).await),
            Err(e) => format!("Batch move failed: {e}"),
        }
    }

    #[tool(
        name = "view_shared",
        description = "View a shared collection using its share token."
    )]
    async fn view_shared(
        &self,
        Parameters(args): Parameters<ViewSharedArgs>,
    ) -> String {
        let r = self.request(endpoints::SHARED_GET, &[("token", &args.share_token)])
            .send().await;
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
                        let authors = meta["authors"].as_array().map(|a| {
                            a.iter().filter_map(|p| p["family"].as_str()).collect::<Vec<_>>().join(", ")
                        }).unwrap_or_default();
                        let year = meta["date"]["year"].as_i64().map(|y| format!(" ({y})")).unwrap_or_default();
                        out.push(format!("- {authors}{year}: {title}"));
                    }
                    if entries > 20 {
                        out.push(format!("... and {} more", entries - 20));
                    }
                }
                out.join("\n")
            }
            Ok(r) if r.status().as_u16() == 404 => "Shared collection not found or link expired.".into(),
            _ => "Failed to load shared collection.".into(),
        }
    }

    // --- Utility tools ---

    #[tool(
        name = "generate_citation_keys",
        description = "Generate Better BibTeX-style citation keys (e.g. 'goswami2026') for a list of DOIs. Requires academic/business plan."
    )]
    async fn generate_citation_keys(
        &self,
        Parameters(args): Parameters<GenerateCitationKeysArgs>,
    ) -> String {
        // Resolve all DOIs to metadata in parallel
        let api_base = self.api_base.clone();
        let futs: Vec<_> = args.dois.iter().map(|doi| {
            let http = self.http.clone();
            let api_base = api_base.clone();
            let doi = doi.clone();
            async move {
                let r = http
                    .post(endpoints::LOOKUP_DOI.url(&api_base, &[]))
                    .json(&serde_json::json!({"doi": doi}))
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status().is_success() => resp.json::<serde_json::Value>().await.ok(),
                    _ => None,
                }
            }
        }).collect();
        let entries: Vec<serde_json::Value> = stream::iter(futs)
            .buffer_unordered(10)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();

        if entries.is_empty() {
            return "Could not resolve any DOIs.".into();
        }

        let r = self.request(endpoints::CITATION_KEYS, &[])
            .json(&serde_json::json!({"entries": entries}))
            .send().await;
        match r {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let keys = data["keys"].as_array().map(|a| {
                    a.iter().filter_map(|k| k.as_str()).collect::<Vec<_>>().join("\n")
                }).unwrap_or_default();
                if keys.is_empty() { "No keys generated.".into() } else { keys }
            }
            Ok(r) => format!("Citation key generation failed: {}", error_detail(r).await),
            Err(e) => format!("Citation key generation failed: {e}"),
        }
    }

    #[tool(
        name = "expand_journal",
        description = "Expand a journal abbreviation to its full name (e.g. 'JACS' -> 'Journal of the American Chemical Society'). 16,000+ journals supported. Requires academic/business plan."
    )]
    async fn expand_journal(
        &self,
        Parameters(args): Parameters<ExpandJournalArgs>,
    ) -> String {
        let r = self.request(endpoints::JOURNAL_EXPAND, &[])
            .json(&serde_json::json!({"abbreviation": args.abbreviation}))
            .send().await;
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
             COLLECTION MANAGEMENT: \
             use list_collections to see saved citation collections. \
             use add_to_collection to save a citation to a named collection (creates if needed). \
             use batch_add_to_collection to add multiple citations at once. \
             use import_bibliography to import BibTeX or RIS files into a collection. \
             use export_collection to get BibTeX for a collection. \
             use search_collection to find entries within a collection. \
             use check_duplicates to check if a citation already exists in a collection. \
             use delete_collection to remove a collection. \
             use update_collection to rename or change a collection's default style. \
             use remove_from_collection to remove a specific entry. \
             use update_tags to set tags on a collection. \
             use reorder_collection to change the order of entries. \
             SHARING: \
             use share_collection to create a shareable link. \
             use unshare_collection to revoke sharing. \
             use view_shared to view a shared collection by token. \
             BULK OPERATIONS: \
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Handle setup subcommand
    if args.iter().any(|a| a == "setup") {
        setup::run(&args[1..]).await;
        return Ok(());
    }

    // Startup checks (logs to stderr, which MCP clients ignore)
    validate_auth().await;
    check_for_updates().await;

    let server = Server::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn validate_auth() {
    let api_key = match std::env::var("OOKCITE_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!(
                "ookcite-mcp: anonymous mode (10 lookups/day). \
                 Set OOKCITE_API_KEY for more."
            );
            return;
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(format!("{API}/api/v1/me"))
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await;

    #[derive(Deserialize)]
    struct MeResponse {
        authenticated: bool,
        plan: String,
        lookups_remaining: u32,
        lookups_limit: u32,
    }

    match resp {
        Ok(r) if r.status().is_success() => match r.json::<MeResponse>().await {
            Ok(me) if me.authenticated => {
                eprintln!(
                    "ookcite-mcp: {} plan, {}/{} lookups remaining",
                    me.plan, me.lookups_remaining, me.lookups_limit
                );
            }
            _ => {
                eprintln!("ookcite-mcp: WARNING: API key not recognized");
            }
        },
        _ => {
            eprintln!("ookcite-mcp: WARNING: could not reach API for key validation");
        }
    }
}

async fn check_for_updates() {
    let current = env!("CARGO_PKG_VERSION");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();

    #[derive(Deserialize)]
    struct NpmPackage {
        #[serde(rename = "dist-tags")]
        dist_tags: std::collections::HashMap<String, String>,
    }

    let resp = client
        .get("https://registry.npmjs.org/@turtletech/ookcite-mcp")
        .header("accept", "application/vnd.npm.install-v1+json")
        .send()
        .await;

    let latest = match resp {
        Ok(r) if r.status().is_success() => {
            r.json::<NpmPackage>()
                .await
                .ok()
                .and_then(|p| p.dist_tags.get("latest").cloned())
        }
        _ => None,
    };

    if let Some(ref latest) = latest {
        if latest != current {
            eprintln!(
                "ookcite-mcp: UPDATE AVAILABLE: v{current} -> v{latest}. \
                 Run: npx @turtletech/ookcite-mcp@latest setup"
            );
        }
    }
}

#[cfg(test)]
impl Server {
    fn new_with_base(api_base: String) -> Self {
        Self {
            tool_router: Self::tool_router(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            api_base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_url_construction() {
        let u = endpoints::LOOKUP_DOI.url("https://example.com", &[]);
        assert_eq!(u, "https://example.com/api/v1/lookup/doi");
    }

    #[test]
    fn test_endpoint_url_with_path_params() {
        let u = endpoints::COLLECTION_ENTRIES_ADD
            .url("https://example.com", &[("id", "abc-123")]);
        assert_eq!(u, "https://example.com/api/v1/collections/abc-123/entries");
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
        assert_eq!(detail, "403 Forbidden: This feature requires academic plan.");
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
        let resp = http::Response::builder()
            .status(500)
            .body("")
            .unwrap();
        let resp = reqwest::Response::from(resp);
        let detail = error_detail(resp).await;
        assert_eq!(detail, "500 Internal Server Error");
    }

    #[tokio::test]
    async fn test_error_detail_long_body_truncated() {
        let long = "x".repeat(200);
        let resp = http::Response::builder()
            .status(502)
            .body(long)
            .unwrap();
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
        let args: FormatArgs = serde_json::from_str(r#"{"doi": "10.1038/187493a0", "style": "ieee"}"#).unwrap();
        assert_eq!(args.style, "ieee");
    }

    #[test]
    fn test_args_import_default_format() {
        let args: ImportBibliographyArgs = serde_json::from_str(r#"{"collection": "test", "content": "@article{...}"}"#).unwrap();
        assert_eq!(args.format, "bibtex");
    }

    #[test]
    fn test_args_import_ris() {
        let args: ImportBibliographyArgs = serde_json::from_str(r#"{"collection": "test", "content": "TY - JOUR", "format": "ris"}"#).unwrap();
        assert_eq!(args.format, "ris");
    }

    #[test]
    fn test_args_batch_add() {
        let args: BatchAddArgs = serde_json::from_str(r#"{"collection": "refs", "queries": ["10.1038/187493a0", "Einstein 1905"]}"#).unwrap();
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
        let args: MergeCollectionsArgs = serde_json::from_str(r#"{"collections": ["a", "b", "c"]}"#).unwrap();
        assert_eq!(args.collections.len(), 3);
    }

    #[test]
    fn test_args_batch_move() {
        let args: BatchMoveArgs = serde_json::from_str(r#"{"source": "a", "target": "b", "entry_ids": ["e1", "e2"]}"#).unwrap();
        assert_eq!(args.source, "a");
        assert_eq!(args.entry_ids.len(), 2);
    }

    // --- Wiremock integration tests ---

    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{body_string_contains, method, path};
    use rmcp::handler::server::wrapper::Parameters;

    fn test_server(base: &str) -> Server {
        Server::new_with_base(base.to_string())
    }

    #[tokio::test]
    async fn test_validate_doi_success() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/187493a0",
                "title": "Stimulated Optical Radiation in Ruby",
                "authors": [{"family": "Maiman", "given": "T. H."}],
                "date": {"year": 1960},
                "journal": "Nature",
                "volume": "187",
                "issue": "4736"
            })))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.validate_doi(Parameters(DoiArgs { doi: "10.1038/187493a0".into() })).await;
        assert!(result.starts_with("VALID"));
        assert!(result.contains("Stimulated Optical Radiation in Ruby"));
        assert!(result.contains("Maiman"));
    }

    #[tokio::test]
    async fn test_validate_doi_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.validate_doi(Parameters(DoiArgs { doi: "10.9999/fake".into() })).await;
        assert!(result.starts_with("INVALID"));
        assert!(!result.contains("CrossRef"));
    }

    #[tokio::test]
    async fn test_validate_doi_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(429)
                .set_body_string("Daily limit reached (50/day). Resets in 3h 45m."))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.validate_doi(Parameters(DoiArgs { doi: "10.1038/187493a0".into() })).await;
        assert!(result.starts_with("RATE LIMITED"));
        assert!(result.contains("Daily limit"));
        assert!(!result.contains("not found"));
    }

    #[tokio::test]
    async fn test_validate_doi_plan_required() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": "plan_required",
                "message": "This feature requires an academic ($4/mo) or business ($12/mo) plan.",
                "upgrade_url": "https://my.turtletech.us"
            })))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.validate_doi(Parameters(DoiArgs { doi: "10.1038/187493a0".into() })).await;
        assert!(result.starts_with("ACCESS DENIED"));
        assert!(result.contains("academic"));
    }

    #[tokio::test]
    async fn test_validate_doi_unauthorized_is_not_reported_as_invalid() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "message": "Authentication required"
            })))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.validate_doi(Parameters(DoiArgs { doi: "10.1038/187493a0".into() })).await;
        assert!(result.starts_with("ACCESS DENIED"));
        assert!(!result.contains("hallucination"));
        assert!(!result.contains("INVALID"));
    }

    #[tokio::test]
    async fn test_verify_references_preserves_client_error_status() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(422).set_body_string("malformed doi payload"))
            .mount(&mock).await;

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
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(503)
                .set_body_string("Lookup service temporarily unavailable. Please try again shortly."))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.validate_doi(Parameters(DoiArgs { doi: "10.1038/187493a0".into() })).await;
        assert!(result.starts_with("TEMPORARY ERROR"));
        assert!(!result.contains("INVALID"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_success() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "metadata": {
                        "title": "Stimulated Optical Radiation in Ruby",
                        "doi": "10.1038/187493a0",
                        "journal": "Nature"
                    },
                    "score": 95.0
                }
            ])))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "Maiman 1960 ruby laser".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
            }))
            .await;
        assert!(result.contains("Stimulated Optical Radiation"));
        assert!(result.contains("10.1038/187493a0"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_no_matches() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "nonexistent paper xyz".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
            }))
            .await;
        assert_eq!(result, "No matches found");
    }

    #[tokio::test]
    async fn test_health_check_success() {
        let mock = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "0.1.0",
                "cache": {"hits": 1234, "misses": 56}
            })))
            .mount(&mock).await;

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
        Mock::given(method("GET")).and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "col-123", "name": "My Refs", "entry_count": 5}
            ])))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.resolve_collection_id("My Refs").await;
        assert_eq!(result, Ok("col-123".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_collection_id_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.resolve_collection_id("Nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_resolve_collection_id_auth_required() {
        let mock = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.resolve_collection_id("anything").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Authentication required"));
    }

    #[tokio::test]
    async fn test_verify_references_parallel() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doi": "10.1038/187493a0",
                "title": "Test Paper"
            })))
            .expect(3)
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.verify_references(Parameters(VerifyArgs {
            dois: vec!["10.1038/1".into(), "10.1038/2".into(), "10.1038/3".into()],
        })).await;
        assert_eq!(result.lines().count(), 3);
        assert!(result.lines().all(|l| l.starts_with("VALID")));
    }

    #[tokio::test]
    async fn test_verify_references_preserves_rate_limit_status() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
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
            .resolve_query_to_metadata("Wright 1931 genetics shifting balance")
            .await
            .expect("metadata");

        assert_eq!(
            metadata["doi"].as_str(),
            Some("10.1093/genetics/16.2.97")
        );
        assert_eq!(
            metadata["title"].as_str(),
            Some("Shifting Balance in Evolution")
        );
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
            }))
            .await;

        assert!(result.contains("Added 1 to 'RuhiMastersThesis'"));
        assert!(!result.contains("Could not resolve"));
    }

    #[tokio::test]
    async fn test_expand_journal_success() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/journal/expand"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "abbreviation": "JACS",
                "full_name": "Journal of the American Chemical Society",
                "found": true
            })))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.expand_journal(Parameters(ExpandJournalArgs { abbreviation: "JACS".into() })).await;
        assert!(result.contains("Journal of the American Chemical Society"));
    }

    #[tokio::test]
    async fn test_expand_journal_not_found() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/journal/expand"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "abbreviation": "XYZ",
                "full_name": null,
                "found": false
            })))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.expand_journal(Parameters(ExpandJournalArgs { abbreviation: "XYZ".into() })).await;
        assert!(result.contains("No expansion found"));
    }

    #[tokio::test]
    async fn test_error_detail_surfaces_plan_gating() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/collections/col-123/entries"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": "plan_required",
                "message": "This feature requires an academic ($4/mo) or business ($12/mo) plan."
            })))
            .mount(&mock).await;

        // Manually call the endpoint to test error_detail
        let client = reqwest::Client::new();
        let resp = client.post(format!("{}/api/v1/collections/col-123/entries", mock.uri()))
            .json(&serde_json::json!({"metadata": {}}))
            .send().await.unwrap();
        let detail = error_detail(resp).await;
        assert!(detail.contains("academic"));
        assert!(detail.contains("$4/mo"));
    }

    #[tokio::test]
    async fn test_format_citation_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(429)
                .set_body_string("Daily limit reached (30/day). Resets in 5h."))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.format_citation(Parameters(FormatArgs { doi: "10.1038/187493a0".into(), style: "apa".into() })).await;
        assert!(result.starts_with("RATE LIMITED"));
        assert!(!result.contains("not found"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(429)
                .set_body_string("Daily limit reached"))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "test".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
            }))
            .await;
        assert!(result.starts_with("RATE LIMITED"));
    }

    #[tokio::test]
    async fn test_reverse_lookup_temporary_upstream_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(503)
                .set_body_string("Lookup service temporarily unavailable. Please try again shortly."))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s
            .reverse_lookup(Parameters(ReverseArgs {
                text: "test".into(),
                author: None,
                journal: None,
                year: None,
                orcid: None,
            }))
            .await;
        assert!(result.starts_with("TEMPORARY ERROR"));
        assert!(!result.contains("No matches"));
    }

    #[tokio::test]
    async fn test_isbn_rate_limited() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/v1/lookup/isbn"))
            .respond_with(ResponseTemplate::new(429)
                .set_body_string("Daily limit reached"))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.lookup_isbn(Parameters(IsbnArgs { isbn: "978-0-521-85629-7".into() })).await;
        assert!(result.starts_with("RATE LIMITED"));
    }

    #[tokio::test]
    async fn test_no_crossref_leak_in_errors() {
        let mock = MockServer::start().await;
        // 404 should not mention CrossRef
        Mock::given(method("POST")).and(path("/api/v1/lookup/doi"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock).await;

        let s = test_server(&mock.uri());
        let result = s.validate_doi(Parameters(DoiArgs { doi: "10.9999/fake".into() })).await;
        assert!(!result.to_lowercase().contains("crossref"), "Error leaked 'CrossRef': {result}");
        assert!(!result.to_lowercase().contains("openlibrary"), "Error leaked 'OpenLibrary': {result}");
    }
}
