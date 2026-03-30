//! # OokCite MCP Server
//!
//! A [Model Context Protocol](https://modelcontextprotocol.io/) server that
//! gives LLMs the ability to validate DOIs, format citations, and catch
//! hallucinated academic references.
//!
//! ## Tools
//!
//! - **validate_doi** -- Check if a DOI exists in CrossRef (anti-hallucination)
//! - **lookup_isbn** -- Look up a book by ISBN via OpenLibrary
//! - **reverse_lookup** -- Find a paper from messy citation text
//! - **format_citation** -- Format a DOI in any of 2900+ CSL styles
//! - **verify_references** -- Batch-check a list of DOIs
//! - **batch_format** -- Resolve and format multiple citations at once
//!
//! ## Usage
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "ookcite": {
//!       "command": "ookcite-mcp"
//!     }
//!   }
//! }
//! ```
//!
//! Connects to the public OokCite API at <https://ookcite.turtletech.us>.
//! No API key required for basic usage.

use rmcp::ServerHandler;
use rmcp::{
    ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::{Json, Parameters}},
    model::*,
    tool, tool_router, tool_handler,
};
use serde::{Deserialize, Serialize};

const API: &str = "https://ookcite-api.turtletech.us";

fn url(path: &str) -> String { format!("{API}{path}") }

#[derive(Clone)]
struct Server {
    tool_router: ToolRouter<Self>,
    http: reqwest::Client,
}

// --- Args ---

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
fn default_style() -> String { "apa".into() }

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

// --- Outputs ---

#[derive(Serialize, schemars::JsonSchema)]
struct TextOutput { text: String }

// --- Tools ---

#[tool_router]
impl Server {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .default_headers({
                    let mut h = reqwest::header::HeaderMap::new();
                    h.insert("origin", "https://ookcite.turtletech.us".parse().unwrap());
                    h
                })
                .build()
                .unwrap(),
        }
    }

    #[tool(name = "validate_doi", description = "Check if a DOI exists in CrossRef and return its metadata. Use this to verify citations are real (anti-hallucination). Returns title, authors, year, journal.")]
    async fn validate_doi(&self, Parameters(args): Parameters<DoiArgs>) -> Json<TextOutput> {
        let r = self.http.post(url("/api/v1/lookup/doi"))
            .json(&serde_json::json!({"doi": args.doi}))
            .send().await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let meta: serde_json::Value = resp.json().await.unwrap_or_default();
                let title = meta["title"].as_str().unwrap_or("?");
                let authors = meta["authors"].as_array()
                    .map(|a| a.iter().filter_map(|x| x["family"].as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();
                let year = meta["date"]["year"].as_i64().map(|y| y.to_string()).unwrap_or_default();
                let doi = meta["doi"].as_str().unwrap_or(&args.doi);
                Json(TextOutput { text: format!("VALID\nDOI: {doi}\nTitle: {title}\nAuthors: {authors}\nYear: {year}") })
            }
            _ => Json(TextOutput { text: format!("INVALID -- DOI {} not found in CrossRef. This citation may be hallucinated.", args.doi) }),
        }
    }

    #[tool(name = "lookup_isbn", description = "Look up a book by ISBN. Returns title, authors, publisher, year.")]
    async fn lookup_isbn(&self, Parameters(args): Parameters<IsbnArgs>) -> Json<TextOutput> {
        let r = self.http.post(url("/api/v1/lookup/isbn"))
            .json(&serde_json::json!({"isbn": args.isbn}))
            .send().await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                Json(TextOutput { text: resp.text().await.unwrap_or_default() })
            }
            _ => Json(TextOutput { text: format!("ISBN {} not found", args.isbn) }),
        }
    }

    #[tool(name = "reverse_lookup", description = "Parse a messy citation string and find the matching paper in CrossRef. Returns ranked candidates. Use when you have citation text in any format.")]
    async fn reverse_lookup(&self, Parameters(args): Parameters<ReverseArgs>) -> Json<TextOutput> {
        let r = self.http.post(url("/api/v1/reverse"))
            .json(&serde_json::json!({"text": args.text}))
            .send().await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let candidates: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                let mut out = Vec::new();
                for (i, c) in candidates.iter().enumerate() {
                    let title = c["metadata"]["title"].as_str().unwrap_or("?");
                    let doi = c["metadata"]["doi"].as_str().unwrap_or("?");
                    let score = c["score"].as_f64().unwrap_or(0.0);
                    out.push(format!("{}. [score:{:.0}] {title} (doi:{doi})", i+1, score));
                }
                Json(TextOutput { text: if out.is_empty() { "No matches found".into() } else { out.join("\n") } })
            }
            _ => Json(TextOutput { text: "Reverse lookup failed".into() }),
        }
    }

    #[tool(name = "format_citation", description = "Format a citation by DOI in a specific CSL style. Returns both the in-text marker (e.g. '(Smith, 2020)' or '[1]') and the full bibliography entry.")]
    async fn format_citation(&self, Parameters(args): Parameters<FormatArgs>) -> Json<TextOutput> {
        // Lookup
        let lookup = self.http.post(url("/api/v1/lookup/doi"))
            .json(&serde_json::json!({"doi": args.doi}))
            .send().await;
        let meta: serde_json::Value = match lookup {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => return Json(TextOutput { text: format!("DOI {} not found", args.doi) }),
        };
        // Format
        let fmt = self.http.post(url("/api/v1/format"))
            .json(&serde_json::json!({"entries": [meta], "style": args.style, "locale": "en-US"}))
            .send().await;
        match fmt {
            Ok(r) if r.status().is_success() => {
                let result: serde_json::Value = r.json().await.unwrap_or_default();
                let plain = result["plain"].as_str().unwrap_or("").trim();
                let intext = result["citations"].as_array()
                    .and_then(|a| a.first())
                    .and_then(|c| c["plain"].as_str())
                    .unwrap_or("");
                Json(TextOutput { text: format!("In-text: {intext}\nReference: {plain}") })
            }
            _ => Json(TextOutput { text: "Format failed".into() }),
        }
    }

    #[tool(name = "verify_references", description = "Batch verify that a list of DOIs exist (anti-hallucination). Returns VALID or INVALID for each. Use before including citations in any document.")]
    async fn verify_references(&self, Parameters(args): Parameters<VerifyArgs>) -> Json<TextOutput> {
        let mut results = Vec::new();
        for doi in &args.dois {
            let r = self.http.post(url("/api/v1/lookup/doi"))
                .json(&serde_json::json!({"doi": doi}))
                .send().await;
            match r {
                Ok(resp) if resp.status().is_success() => {
                    let meta: serde_json::Value = resp.json().await.unwrap_or_default();
                    let title = meta["title"].as_str().unwrap_or("?");
                    results.push(format!("VALID {doi} -- {title}"));
                }
                _ => results.push(format!("INVALID {doi} -- NOT FOUND (possibly hallucinated)")),
            }
        }
        Json(TextOutput { text: results.join("\n") })
    }

    #[tool(name = "batch_format", description = "Resolve and format multiple messy citations at once. Pass citation strings in any format. Returns formatted references with in-text markers and correct sequential numbering.")]
    async fn batch_format(&self, Parameters(args): Parameters<BatchArgs>) -> Json<TextOutput> {
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for (i, text) in args.citations.iter().enumerate() {
            let r = self.http.post(url("/api/v1/reverse"))
                .json(&serde_json::json!({"text": text}))
                .send().await;
            match r {
                Ok(resp) if resp.status().is_success() => {
                    let candidates: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                    if let Some(meta) = candidates.first().and_then(|c| c.get("metadata")) {
                        entries.push(meta.clone());
                    } else {
                        errors.push(format!("[{}] Not found: {}", i+1, &text[..text.len().min(60)]));
                    }
                }
                _ => errors.push(format!("[{}] Failed: {}", i+1, &text[..text.len().min(60)])),
            }
        }
        if entries.is_empty() {
            return Json(TextOutput { text: format!("No citations resolved.\n{}", errors.join("\n")) });
        }
        let fmt = self.http.post(url("/api/v1/format"))
            .json(&serde_json::json!({"entries": entries, "style": args.style, "locale": "en-US"}))
            .send().await;
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
                    out.push("\n--- Unresolved ---".into());
                    out.extend(errors);
                }
                Json(TextOutput { text: out.join("\n") })
            }
            _ => Json(TextOutput { text: "Batch format failed".into() }),
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
             Use validate_doi to check if citations are real (anti-hallucination). \
             Use format_citation to get properly formatted references in any CSL style. \
             Use verify_references to batch-check a list of DOIs before including them.".into()
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = Server::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
