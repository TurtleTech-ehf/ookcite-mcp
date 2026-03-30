//! # OokCite MCP Server
//!
//! A [Model Context Protocol](https://modelcontextprotocol.io/) server that
//! gives LLMs the ability to validate DOIs, format citations, and catch
//! hallucinated academic references.
//!
//! ## Tools
//!
//! * **validate_doi** : Check if a DOI exists in CrossRef (anti-hallucination)
//! * **lookup_isbn** : Look up a book by ISBN via OpenLibrary
//! * **reverse_lookup** : Find a paper from messy citation text
//! * **format_citation** : Format a DOI in any of 2900+ CSL styles
//! * **verify_references** : Batch-check a list of DOIs
//! * **batch_format** : Resolve and format multiple citations at once
//! * **search_styles** : Find CSL style IDs by name
//! * **group_cite** : Generate grouped in-text markers
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
use serde::Deserialize;

const API: &str = "https://ookcite-api.turtletech.us";

fn url(path: &str) -> String {
    format!("{API}{path}")
}

#[derive(Clone)]
struct Server {
    tool_router: ToolRouter<Self>,
    http: reqwest::Client,
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
    /// Messy citation text to parse
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

// Tools

#[tool_router]
impl Server {
    fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("origin", "https://ookcite.turtletech.us".parse().unwrap());

        if let Ok(api_key) = std::env::var("OOKCITE_API_KEY") {
            if let Ok(mut auth_val) =
                format!("Bearer {api_key}").parse::<reqwest::header::HeaderValue>()
            {
                auth_val.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, auth_val);
            }
        }

        Self {
            tool_router: Self::tool_router(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .default_headers(headers)
                .build()
                .unwrap(),
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
        let req_url = url(&format!(
            "/api/v1/styles/search?q={}",
            urlencoding::encode(&args.query)
        ));
        let r = self.http.get(&req_url).send().await;
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
        description = "Check if a DOI exists in CrossRef and return its metadata. Use this to verify citations. Returns title, authors, year, journal, volume, and issue."
    )]
    async fn validate_doi(&self, Parameters(args): Parameters<DoiArgs>) -> String {
        let r = self
            .http
            .post(url("/api/v1/lookup/doi"))
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
            _ => format!(
                "INVALID: DOI {} not found in CrossRef. This citation may represent a hallucination.",
                args.doi
            ),
        }
    }

    #[tool(
        name = "lookup_isbn",
        description = "Look up a book by ISBN. Returns title, authors, publisher, year, and pages."
    )]
    async fn lookup_isbn(&self, Parameters(args): Parameters<IsbnArgs>) -> String {
        let r = self
            .http
            .post(url("/api/v1/lookup/isbn"))
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
            _ => format!("ISBN {} not found", args.isbn),
        }
    }

    #[tool(
        name = "reverse_lookup",
        description = "Parse a messy citation string and find the matching paper in CrossRef. Returns ranked candidates."
    )]
    async fn reverse_lookup(&self, Parameters(args): Parameters<ReverseArgs>) -> String {
        let r = self
            .http
            .post(url("/api/v1/reverse"))
            .json(&serde_json::json!({"text": args.text}))
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
            _ => "Reverse lookup failed".into(),
        }
    }

    #[tool(
        name = "format_citation",
        description = "Format a citation by DOI in a specific CSL style. Returns both the in-text marker and the full bibliography entry."
    )]
    async fn format_citation(&self, Parameters(args): Parameters<FormatArgs>) -> String {
        let lookup = self
            .http
            .post(url("/api/v1/lookup/doi"))
            .json(&serde_json::json!({"doi": args.doi}))
            .send()
            .await;
        let meta: serde_json::Value = match lookup {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => return format!("DOI {} not found", args.doi),
        };

        let fmt = self
            .http
            .post(url("/api/v1/format"))
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
        let mut entries = Vec::new();
        for doi in &args.dois {
            let r = self
                .http
                .post(url("/api/v1/lookup/doi"))
                .json(&serde_json::json!({"doi": doi}))
                .send()
                .await;
            if let Ok(resp) = r {
                if resp.status().is_success() {
                    if let Ok(meta) = resp.json::<serde_json::Value>().await {
                        entries.push(meta);
                    }
                }
            }
        }

        if entries.is_empty() {
            return "Failed to resolve any DOIs.".into();
        }

        let indices: Vec<usize> = (0..entries.len()).collect();
        let r = self
            .http
            .post(url("/api/v1/format/group-cite"))
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
        let mut results = Vec::new();
        for doi in &args.dois {
            let r = self
                .http
                .post(url("/api/v1/lookup/doi"))
                .json(&serde_json::json!({"doi": doi}))
                .send()
                .await;
            match r {
                Ok(resp) if resp.status().is_success() => {
                    let meta: serde_json::Value = resp.json().await.unwrap_or_default();
                    let title = meta["title"].as_str().unwrap_or("?");
                    results.push(format!("VALID {doi} : {title}"));
                }
                _ => results.push(format!("INVALID {doi} : NOT FOUND")),
            }
        }
        results.join("\n")
    }

    #[tool(
        name = "batch_format",
        description = "Resolve and format multiple messy citations at once. Pass citation strings in any format."
    )]
    async fn batch_format(&self, Parameters(args): Parameters<BatchArgs>) -> String {
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for (i, text) in args.citations.iter().enumerate() {
            let r = self
                .http
                .post(url("/api/v1/reverse"))
                .json(&serde_json::json!({"text": text}))
                .send()
                .await;
            match r {
                Ok(resp) if resp.status().is_success() => {
                    let candidates: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                    if let Some(meta) = candidates.first().and_then(|c| c.get("metadata")) {
                        entries.push(meta.clone());
                    } else {
                        errors.push(format!(
                            "[{}] Not found: {}",
                            i + 1,
                            &text[..text.len().min(60)]
                        ));
                    }
                }
                _ => errors.push(format!(
                    "[{}] Failed: {}",
                    i + 1,
                    &text[..text.len().min(60)]
                )),
            }
        }
        if entries.is_empty() {
            return format!("No citations resolved.\n{}", errors.join("\n"));
        }
        let fmt = self
            .http
            .post(url("/api/v1/format"))
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
            _ => "Batch format failed".into(),
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
            "OokCite: citation validation and formatting for LLMs. \
             Use search_styles to locate specific CSL configurations like 'american-chemical-society'. \
             Use validate_doi to check if citations actually exist in the scientific record. \
             Use format_citation or group_cite to generate properly formatted reference markers.".into()
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

    // Startup auth validation (logs to stderr, which MCP clients ignore)
    validate_auth().await;

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
        .header("origin", "https://ookcite.turtletech.us")
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
