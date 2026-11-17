//! Ridley library MCP tools.
//!
//! Local mode delegates reads to `tt`, which owns the ridley-core replica
//! integration and can therefore open sealed objects. It is the default and
//! primary read path. Remote mode is an explicit metadata-only fallback.

use std::path::PathBuf;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use crate::inbound_auth::{self, apply_bearer};
use crate::policy::block_mutate;
use crate::server::Server;

const DEFAULT_DASHBOARD_API: &str = "https://dashboard-api.turtletech.us/api/v1";
pub(crate) const SAVE_INBOX_UNAVAILABLE: &str =
    "Ridley save inbox isn't available on this server yet.";

#[cfg(test)]
tokio::task_local! {
    static TEST_DASHBOARD_API: String;
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ListLibrariesArgs {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchLibraryArgs {
    /// Library id or its exact display name.
    pub library: String,
    /// General metadata query. Combined with the field filters below.
    #[serde(default)]
    pub query: String,
    /// Words that must occur in the title.
    #[serde(default)]
    pub title: Option<String>,
    /// Author name to find.
    #[serde(default)]
    pub author: Option<String>,
    /// Tag to find.
    #[serde(default)]
    pub tag: Option<String>,
    /// Search locally decrypted full text as well as metadata. This requires
    /// local mode because the dashboard cannot open sealed objects.
    #[serde(default)]
    pub full_text: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetLibraryItemArgs {
    /// Library id or its exact display name.
    pub library: String,
    /// Stable item id returned by search_library.
    pub item_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddByDoiArgs {
    /// Library id or its exact display name.
    pub library: String,
    /// DOI to resolve and add.
    pub doi: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExportLibraryBibArgs {
    /// Library id or its exact display name.
    pub library: String,
    /// Collection id or exact collection name within the library.
    pub collection: String,
}

#[derive(Clone, Debug)]
enum LibraryMode {
    Remote,
    Local { replica: PathBuf, tt: PathBuf },
}

impl LibraryMode {
    fn from_environment(shared_http: bool) -> Result<Self, String> {
        let mode = std::env::var("OOKCITE_RIDLEY_MODE").ok();
        let replica = std::env::var("OOKCITE_RIDLEY_REPLICA")
            .ok()
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        let tt = std::env::var("OOKCITE_RIDLEY_TT")
            .ok()
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        Self::from_config(shared_http, mode.as_deref(), replica, tt)
    }

    fn from_config(
        shared_http: bool,
        mode: Option<&str>,
        replica: Option<PathBuf>,
        tt: Option<PathBuf>,
    ) -> Result<Self, String> {
        if shared_http {
            if replica.is_some()
                || mode.is_some_and(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "local" | "replica"
                    )
                })
            {
                eprintln!(
                    "ookcite-mcp warning: ignoring local Ridley replica configuration \
                     on a shared HTTP server"
                );
            }
            return Ok(Self::Remote);
        }
        let mode = mode.unwrap_or("local").trim().to_ascii_lowercase();
        match mode.as_str() {
            "" | "remote" | "dashboard" => Ok(Self::Remote),
            "local" | "replica" => {
                let replica = replica.ok_or_else(|| {
                    "Local Ridley mode requires OOKCITE_RIDLEY_REPLICA.".to_string()
                })?;
                let tt = tt.unwrap_or_else(|| PathBuf::from("tt"));
                Ok(Self::Local { replica, tt })
            }
            other => Err(format!(
                "OOKCITE_RIDLEY_MODE must be remote or local, got {other}"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct LibraryHome {
    #[serde(default)]
    libraries: Vec<LibrarySummary>,
}

#[derive(Debug, Deserialize)]
struct RidleyLibraries {
    #[serde(default)]
    libraries: Vec<RidleyLibrarySummary>,
}

#[derive(Clone, Debug, Deserialize)]
struct RidleyLibrarySummary {
    library_id: String,
    #[serde(default)]
    scope: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LibrarySummary {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct TablePage {
    #[serde(default)]
    items: Vec<TableItem>,
}

#[derive(Clone, Debug, Deserialize)]
struct TableItem {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    authors: String,
    #[serde(default)]
    year: String,
    #[serde(default, rename = "item_type")]
    kind: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    doi: String,
    #[serde(default)]
    publication: String,
}

#[derive(Debug, Deserialize)]
struct ItemPage {
    #[serde(default)]
    page: u32,
    #[serde(default)]
    page_size: u32,
    #[serde(default)]
    total: u32,
    #[serde(default)]
    collections: Vec<Collection>,
    #[serde(default)]
    items: Vec<LibraryItem>,
}

#[derive(Clone, Debug, Deserialize)]
struct Collection {
    id: String,
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LibraryItem {
    id: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    collections: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ExportFile {
    #[serde(default)]
    body: String,
}

#[tool_router(router = ridley_tool_router, vis = "pub(crate)")]
impl Server {
    #[tool(
        name = "list_libraries",
        description = "List Ridley libraries visible to the signed-in user. This catalogue-only operation uses the setup --connect bearer so the dashboard scopes the response to that user.",
        annotations(
            title = "List Ridley libraries",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_libraries(
        &self,
        #[allow(unused)] Parameters(_args): Parameters<ListLibrariesArgs>,
    ) -> String {
        self.remote_list_libraries().await
    }

    #[tool(
        name = "search_library",
        description = "Search a local Ridley replica by title, author, tag, notes, highlights, full text, DOI, or publication. Local mode is the default because only ridley-core can open sealed objects. Explicit remote mode searches catalogue metadata only.",
        annotations(
            title = "Search a Ridley library",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn search_library(&self, Parameters(args): Parameters<SearchLibraryArgs>) -> String {
        match LibraryMode::from_environment(self.is_shared_http()) {
            Ok(LibraryMode::Remote) => self.remote_search_library(&args).await,
            Ok(LibraryMode::Local { replica, tt }) => {
                let query = combined_query(&args);
                let mut owned = vec![
                    "library".to_string(),
                    "search".to_string(),
                    query,
                    "--library".to_string(),
                    args.library,
                    "--json".to_string(),
                ];
                if args.full_text {
                    owned.push("--full-text".into());
                }
                let refs = owned.iter().map(String::as_str).collect::<Vec<_>>();
                run_tt(&tt, &replica, &refs).await
            }
            Err(error) => error,
        }
    }

    #[tool(
        name = "get_item",
        description = "Get one Ridley item with metadata, notes, highlights, and full text from the local ridley-core replica. Explicit remote mode can return catalogue metadata only because the dashboard cannot open sealed objects.",
        annotations(
            title = "Get a Ridley item",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn get_library_item(&self, Parameters(args): Parameters<GetLibraryItemArgs>) -> String {
        match LibraryMode::from_environment(self.is_shared_http()) {
            Ok(LibraryMode::Remote) => self.remote_get_item(&args).await,
            Ok(LibraryMode::Local { replica, tt }) => {
                run_tt(
                    &tt,
                    &replica,
                    &[
                        "library",
                        "get",
                        &args.item_id,
                        "--library",
                        &args.library,
                        "--json",
                    ],
                )
                .await
            }
            Err(error) => error,
        }
    }

    #[tool(
        name = "add_by_doi",
        description = "Queue a DOI in a Ridley library's dashboard save inbox. The next Ridley client sync resolves and writes it into the encrypted replica. Requires the signed-in setup --connect credential and mutation policy.",
        annotations(
            title = "Add DOI to Ridley",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn add_by_doi(&self, Parameters(args): Parameters<AddByDoiArgs>) -> String {
        if let Some(message) = block_mutate() {
            return message;
        }
        self.remote_add_by_doi(&args).await
    }

    #[tool(
        name = "export_bib",
        description = "Export one collection from the local ridley-core replica as BibTeX. Local mode is the default so export sees the synced library rather than the dashboard share catalogue; explicit remote mode is metadata-only.",
        annotations(
            title = "Export a Ridley collection",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn export_library_bib(
        &self,
        Parameters(args): Parameters<ExportLibraryBibArgs>,
    ) -> String {
        match LibraryMode::from_environment(self.is_shared_http()) {
            Ok(LibraryMode::Remote) => self.remote_export_bib(&args).await,
            Ok(LibraryMode::Local { replica, tt }) => {
                run_tt(
                    &tt,
                    &replica,
                    &[
                        "library",
                        "export",
                        "--collection",
                        &args.collection,
                        "--library",
                        &args.library,
                        "--format",
                        "bibtex",
                    ],
                )
                .await
            }
            Err(error) => error,
        }
    }
}

impl Server {
    fn dashboard_base() -> String {
        #[cfg(test)]
        if let Ok(base) = TEST_DASHBOARD_API.try_with(Clone::clone) {
            return base;
        }
        std::env::var("OOKCITE_DASHBOARD_API")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_DASHBOARD_API.into())
    }

    fn dashboard_request(&self, method: &str, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", Self::dashboard_base(), path);
        let request = match method {
            "GET" => self.http.get(url),
            "POST" => self.http.post(url),
            other => panic!("unsupported dashboard method {other}"),
        };
        apply_bearer(request)
    }

    async fn remote_list(&self) -> Result<LibraryHome, String> {
        if !inbound_auth::has_api_key() {
            return Err(inbound_auth::auth_required_text().into());
        }
        let response = self
            .dashboard_request("GET", "/libraries")
            .send()
            .await
            .map_err(|error| format!("Failed to list Ridley libraries: {error}"))?;
        if !response.status().is_success() {
            return Err(dashboard_failure(response, "Failed to list Ridley libraries").await);
        }
        response
            .json()
            .await
            .map_err(|_| "Dashboard returned an invalid library list.".into())
    }

    async fn remote_ridley_list(&self) -> Result<RidleyLibraries, String> {
        if !inbound_auth::has_api_key() {
            return Err(inbound_auth::auth_required_text().into());
        }
        let response = self
            .dashboard_request("GET", "/ridley/libraries")
            .send()
            .await
            .map_err(|error| format!("Failed to list Ridley libraries: {error}"))?;
        if !response.status().is_success() {
            return Err(dashboard_failure(response, "Failed to list Ridley libraries").await);
        }
        response
            .json()
            .await
            .map_err(|_| "Dashboard returned an invalid Ridley library list.".into())
    }

    async fn resolve_library(&self, needle: &str) -> Result<LibrarySummary, String> {
        let home = self.remote_list().await?;
        let needle = needle.trim();
        let mut matches = home
            .libraries
            .into_iter()
            .filter(|library| library.id == needle || library.name.eq_ignore_ascii_case(needle))
            .collect::<Vec<_>>();
        match matches.len() {
            0 => Err(format!(
                "No accessible Ridley library named or id '{needle}'."
            )),
            1 => Ok(matches.remove(0)),
            _ => Err(format!(
                "More than one accessible Ridley library is named '{needle}'; pass its id."
            )),
        }
    }

    async fn remote_list_libraries(&self) -> String {
        match self.remote_ridley_list().await {
            Ok(home) if home.libraries.is_empty() => "No Ridley libraries found.".into(),
            Ok(home) => home
                .libraries
                .iter()
                .map(|library| {
                    format!(
                        "- {} (scope: {})",
                        library.library_id,
                        nonempty(&library.scope, "unknown")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(error) => error,
        }
    }

    async fn remote_table(&self, library_id: &str, query: &str) -> Result<TablePage, String> {
        let response = self
            .dashboard_request("GET", &format!("/libraries/{}/table", encode(library_id)))
            .query(&[
                ("q", query),
                ("sort", "title"),
                ("dir", "asc"),
                ("columns", "title,authors,year,type,tags,doi,publication"),
            ])
            .send()
            .await
            .map_err(|error| format!("Failed to search Ridley library: {error}"))?;
        if !response.status().is_success() {
            return Err(dashboard_failure(response, "Failed to search Ridley library").await);
        }
        response
            .json()
            .await
            .map_err(|_| "Dashboard returned an invalid library table.".into())
    }

    async fn remote_search_library(&self, args: &SearchLibraryArgs) -> String {
        let library = match self.resolve_library(&args.library).await {
            Ok(library) => library,
            Err(error) => return error,
        };
        let query = combined_query(args);
        let page = match self.remote_table(&library.id, &query).await {
            Ok(page) => page,
            Err(error) => return error,
        };
        let mut lines = page.items.iter().map(format_table_item).collect::<Vec<_>>();
        if args.full_text {
            lines.insert(
                0,
                "Remote results cover metadata only. Full text is sealed; set OOKCITE_RIDLEY_MODE=local and OOKCITE_RIDLEY_REPLICA to search the decrypted local replica.".into(),
            );
        }
        if page.items.is_empty() {
            lines.push(format!("No matches in '{}'.", library.name));
        }
        lines.join("\n")
    }

    async fn item_pages(
        &self,
        library_id: &str,
    ) -> Result<(Vec<LibraryItem>, Vec<Collection>), String> {
        let mut page_number = 1u32;
        let mut items = Vec::new();
        let mut collections = Vec::new();
        loop {
            let response = self
                .dashboard_request("GET", &format!("/libraries/{}/items", encode(library_id)))
                .query(&[("page", page_number)])
                .send()
                .await
                .map_err(|error| format!("Failed to load Ridley items: {error}"))?;
            if !response.status().is_success() {
                return Err(dashboard_failure(response, "Failed to load Ridley items").await);
            }
            let page: ItemPage = response
                .json()
                .await
                .map_err(|_| "Dashboard returned an invalid item page.".to_string())?;
            if collections.is_empty() {
                collections = page.collections;
            }
            items.extend(page.items);
            let loaded = u32::try_from(items.len()).unwrap_or(u32::MAX);
            if loaded >= page.total || page.page_size == 0 || page.page == 0 {
                break;
            }
            page_number = page_number.saturating_add(1);
            if page_number > 10_000 {
                return Err("Dashboard item pagination did not terminate.".into());
            }
        }
        Ok((items, collections))
    }

    async fn remote_get_item(&self, args: &GetLibraryItemArgs) -> String {
        let library = match self.resolve_library(&args.library).await {
            Ok(library) => library,
            Err(error) => return error,
        };
        let table = match self.remote_table(&library.id, "").await {
            Ok(table) => table,
            Err(error) => return error,
        };
        let Some(metadata) = table.items.iter().find(|item| item.id == args.item_id) else {
            return format!(
                "Item '{}' was not found in Ridley library '{}'.",
                args.item_id, library.name
            );
        };
        let (items, collections) = match self.item_pages(&library.id).await {
            Ok(value) => value,
            Err(error) => return error,
        };
        let item = items.iter().find(|item| item.id == args.item_id);
        let collection_names = item
            .map(|item| {
                item.collections
                    .iter()
                    .map(|id| {
                        collections
                            .iter()
                            .find(|collection| collection.id == *id)
                            .map(|collection| collection.name.as_str())
                            .unwrap_or(id)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let tags = item
            .map(|item| item.tags.join(", "))
            .unwrap_or_else(|| metadata.tags.join(", "));
        format!(
            "{}\nTags: {}\nCollections: {}\nNotes: unavailable in remote mode (sealed)\nHighlights: unavailable in remote mode (sealed)\nFull text: unavailable in remote mode (sealed)\nUse OOKCITE_RIDLEY_MODE=local with OOKCITE_RIDLEY_REPLICA to open these fields.",
            format_table_item(metadata),
            nonempty(&tags, "(none)"),
            nonempty(&collection_names, "(none)")
        )
    }

    async fn remote_add_by_doi(&self, args: &AddByDoiArgs) -> String {
        if !inbound_auth::has_api_key() {
            return inbound_auth::auth_required_text().into();
        }
        let libraries = match self.remote_ridley_list().await {
            Ok(libraries) => libraries,
            Err(error) => return error,
        };
        let needle = args.library.trim();
        let mut matches = libraries
            .libraries
            .into_iter()
            .filter(|library| {
                library.library_id == needle
                    || (library.scope == "personal"
                        && matches!(needle.to_ascii_lowercase().as_str(), "personal" | "my"))
            })
            .collect::<Vec<_>>();
        let library = match matches.len() {
            0 => {
                return format!(
                    "No accessible synced Ridley library id '{}'. Call list_libraries first.",
                    args.library
                );
            }
            1 => matches.remove(0),
            _ => return "More than one Ridley library matched; pass its library id.".into(),
        };
        let doi = args
            .doi
            .trim()
            .trim_start_matches("https://doi.org/")
            .trim_start_matches("doi:")
            .trim();
        if !doi.starts_with("10.") || !doi.contains('/') {
            return format!("Invalid DOI '{}'.", args.doi);
        }
        let response = self
            .dashboard_request(
                "POST",
                &format!("/ridley/libraries/{}/inbox", encode(&library.library_id)),
            )
            .json(&serde_json::json!({ "doi": doi }))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                let body: serde_json::Value = response.json().await.unwrap_or_default();
                format!("Queued DOI {} for '{}': {}", doi, library.library_id, body)
            }
            Ok(response) => inbox_failure(response).await,
            Err(error) => format!("Failed to add DOI: {error}"),
        }
    }

    async fn remote_export_bib(&self, args: &ExportLibraryBibArgs) -> String {
        let library = match self.resolve_library(&args.library).await {
            Ok(library) => library,
            Err(error) => return error,
        };
        let (items, collections) = match self.item_pages(&library.id).await {
            Ok(value) => value,
            Err(error) => return error,
        };
        let Some(collection) = collections.iter().find(|collection| {
            collection.id == args.collection
                || collection.name.eq_ignore_ascii_case(args.collection.trim())
        }) else {
            return format!(
                "Collection '{}' was not found in Ridley library '{}'.",
                args.collection, library.name
            );
        };
        let item_ids = items
            .iter()
            .filter(|item| item.collections.contains(&collection.id))
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        if item_ids.is_empty() {
            return format!("Collection '{}' is empty.", collection.name);
        }
        let response = self
            .dashboard_request(
                "POST",
                &format!("/libraries/{}/table/export", encode(&library.id)),
            )
            .json(&serde_json::json!({
                "format": "bibtex",
                "query": "",
                "item_type": "",
                "sort": "title",
                "dir": "asc",
                "columns": ["title", "authors", "year", "tags", "doi"],
                "item_ids": item_ids
            }))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => response
                .json::<ExportFile>()
                .await
                .map(|file| file.body)
                .unwrap_or_else(|_| "Dashboard returned an invalid BibTeX export.".into()),
            Ok(response) => dashboard_failure(response, "Failed to export collection").await,
            Err(error) => format!("Failed to export collection: {error}"),
        }
    }
}

fn combined_query(args: &SearchLibraryArgs) -> String {
    [
        Some(args.query.trim()).filter(|value| !value.is_empty()),
        args.title
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        args.author
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        args.tag
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(index, value)| {
        value.map(|value| match index {
            1 => format!("title:{value}"),
            2 => format!("author:{value}"),
            3 => format!("tag:{value}"),
            _ => value.to_string(),
        })
    })
    .collect::<Vec<String>>()
    .join(" ")
}

fn format_table_item(item: &TableItem) -> String {
    format!(
        "item_id={} | {} | {} | {} | {} | DOI {} | tags [{}] | {}",
        item.id,
        nonempty(&item.title, "(untitled)"),
        nonempty(&item.authors, "(unknown author)"),
        nonempty(&item.year, "(no year)"),
        nonempty(&item.kind, "(unknown type)"),
        nonempty(&item.doi, "(none)"),
        item.tags.join(", "),
        nonempty(&item.publication, "(no publication)")
    )
}

fn nonempty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

async fn dashboard_failure(response: reqwest::Response, prefix: &str) -> String {
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    let code = body["error"].as_str().unwrap_or("dashboard_error");
    match status.as_u16() {
        401 => inbound_auth::auth_required_text().into(),
        403 if inbound_auth::api_key().is_some_and(|key| key.starts_with("ookc_")) => format!(
            "{prefix}: Ridley dashboard access requires the OAuth/session bearer from `ookcite-mcp setup --connect`; OokCite API keys are not accepted."
        ),
        403 => format!(
            "{prefix}: access denied ({code}). Reconnect with `ookcite-mcp setup --connect` to refresh the OAuth/session bearer."
        ),
        404 => format!("{prefix}: library or item not found."),
        _ => format!("{prefix}: dashboard returned HTTP {status} ({code})."),
    }
}

async fn inbox_failure(response: reqwest::Response) -> String {
    let status = response.status();
    if status != reqwest::StatusCode::NOT_FOUND {
        return dashboard_failure(response, "Failed to add DOI").await;
    }
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    match body["error"].as_str() {
        Some("library_not_found" | "ridley_library_not_found") => {
            "Failed to add DOI: Ridley library not found.".into()
        }
        _ => SAVE_INBOX_UNAVAILABLE.into(),
    }
}

async fn run_tt(tt: &PathBuf, replica: &PathBuf, args: &[&str]) -> String {
    let output = tokio::process::Command::new(tt)
        .args(args)
        .arg("--replica")
        .arg(replica)
        .env("TT_RIDLEY_REPLICA", replica)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                "tt returned no library data.".into()
            } else {
                stdout
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!(
                "Local Ridley command failed ({}): {}",
                output.status,
                stderr.trim()
            )
        }
        Err(error) => format!(
            "Could not start '{}': {error}. Install tt or set OOKCITE_RIDLEY_TT.",
            tt.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const KEY: &str = "ookc_library-test-key";

    fn home() -> serde_json::Value {
        serde_json::json!({
            "libraries": [{
                "id": "lib-lab",
                "name": "Lab",
                "access": "owner",
                "bytes": 10,
                "counts_against_you": true,
                "members": [],
                "pending": []
            }],
            "revoked": [],
            "invites": []
        })
    }

    fn ridley_home() -> serde_json::Value {
        serde_json::json!({
            "libraries": [{
                "library_id": "lib-lab",
                "scope": "group",
                "s3": {},
                "encryption": {"key_b64": "not-read-by-this-client", "wire_version": 3},
                "log_migration": {"state": "moved", "ready_at": null}
            }],
            "ridley_log_fence": 2
        })
    }

    async fn authenticated<T>(base: String, future: impl std::future::Future<Output = T>) -> T {
        TEST_DASHBOARD_API
            .scope(
                base,
                inbound_auth::with_http_bearer(Some(KEY.into()), future),
            )
            .await
    }

    async fn mount_home(mock: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/libraries"))
            .and(header("authorization", format!("Bearer {KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(home()))
            .mount(mock)
            .await;
    }

    async fn mount_ridley_home(mock: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/ridley/libraries"))
            .and(header("authorization", format!("Bearer {KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(ridley_home()))
            .mount(mock)
            .await;
    }

    #[test]
    fn query_combines_supported_metadata_fields() {
        let args = SearchLibraryArgs {
            library: "lab".into(),
            query: "attention".into(),
            title: Some("transformer".into()),
            author: Some("Vaswani".into()),
            tag: Some("read".into()),
            full_text: false,
        };
        assert_eq!(
            combined_query(&args),
            "attention title:transformer author:Vaswani tag:read"
        );
    }

    #[test]
    fn shared_http_forces_remote_mode_even_when_a_replica_is_configured() {
        for mode in [None, Some("local")] {
            let selected = LibraryMode::from_config(
                true,
                mode,
                Some(PathBuf::from("/srv/operator/library.redb")),
                Some(PathBuf::from("tt")),
            )
            .unwrap();
            assert!(matches!(selected, LibraryMode::Remote));
        }
    }

    #[test]
    fn ridley_router_registers_all_five_tools() {
        let names = Server::ridley_tool_router()
            .list_all()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        for expected in [
            "list_libraries",
            "search_library",
            "get_item",
            "add_by_doi",
            "export_bib",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn list_libraries_uses_the_scoped_dashboard_bearer() {
        let mock = MockServer::start().await;
        mount_ridley_home(&mock).await;
        let server = Server::new();
        let result = authenticated(mock.uri(), server.remote_list_libraries()).await;
        assert!(result.contains("lib-lab"));
        assert!(result.contains("scope: group"));
    }

    #[tokio::test]
    async fn search_library_reads_dashboard_metadata_when_explicitly_remote() {
        let mock = MockServer::start().await;
        mount_home(&mock).await;
        Mock::given(method("GET"))
            .and(path("/libraries/lib-lab/table"))
            .and(header("authorization", format!("Bearer {KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{
                    "id": "paper-1",
                    "title": "Attention Is All You Need",
                    "authors": "Vaswani et al.",
                    "year": "2017",
                    "item_type": "journalArticle",
                    "tags": ["transformer"],
                    "doi": "10.5555/3295222.3295349",
                    "publication": "NeurIPS"
                }]
            })))
            .mount(&mock)
            .await;
        let server = Server::new();
        let result = authenticated(
            mock.uri(),
            server.remote_search_library(&SearchLibraryArgs {
                library: "Lab".into(),
                query: "transformer".into(),
                title: None,
                author: None,
                tag: None,
                full_text: true,
            }),
        )
        .await;
        assert!(result.contains("Attention Is All You Need"));
        assert!(result.contains("metadata only"));
    }

    #[tokio::test]
    async fn api_key_only_remote_mode_requests_setup_connect_session_bearer() {
        let mock = MockServer::start().await;
        mount_home(&mock).await;
        Mock::given(method("GET"))
            .and(path("/libraries/lib-lab/table"))
            .and(header("authorization", format!("Bearer {KEY}")))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(serde_json::json!({"error": "forbidden"})),
            )
            .mount(&mock)
            .await;
        let server = Server::new();
        let result = authenticated(
            mock.uri(),
            server.remote_search_library(&SearchLibraryArgs {
                library: "Lab".into(),
                query: "transformer".into(),
                title: None,
                author: None,
                tag: None,
                full_text: false,
            }),
        )
        .await;
        assert!(result.contains("OAuth/session bearer"));
        assert!(result.contains("setup --connect"));
        assert!(!result.contains("access denied (forbidden)"));
    }

    #[tokio::test]
    async fn get_item_reports_sealed_fields_in_remote_mode() {
        let mock = MockServer::start().await;
        mount_home(&mock).await;
        Mock::given(method("GET"))
            .and(path("/libraries/lib-lab/table"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{
                    "id": "paper-1",
                    "title": "Paper",
                    "authors": "Ada",
                    "year": "2026",
                    "item_type": "journalArticle",
                    "tags": ["read"],
                    "doi": "10.1000/paper",
                    "publication": "Journal"
                }]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/libraries/lib-lab/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1,
                "page_size": 50,
                "total": 1,
                "collections": [{"id": "reading", "name": "Reading"}],
                "items": [{
                    "id": "paper-1",
                    "title": "Paper",
                    "tags": ["read"],
                    "collections": ["reading"]
                }]
            })))
            .mount(&mock)
            .await;
        let server = Server::new();
        let result = authenticated(
            mock.uri(),
            server.remote_get_item(&GetLibraryItemArgs {
                library: "Lab".into(),
                item_id: "paper-1".into(),
            }),
        )
        .await;
        assert!(result.contains("Collections: Reading"));
        assert!(result.contains("Highlights: unavailable in remote mode (sealed)"));
    }

    #[tokio::test]
    async fn export_bib_selects_only_collection_members() {
        let mock = MockServer::start().await;
        mount_home(&mock).await;
        Mock::given(method("GET"))
            .and(path("/libraries/lib-lab/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1,
                "page_size": 50,
                "total": 2,
                "collections": [{"id": "reading", "name": "Reading"}],
                "items": [
                    {"id": "paper-1", "title": "Paper", "tags": [], "collections": ["reading"]},
                    {"id": "paper-2", "title": "Other", "tags": [], "collections": []}
                ]
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/libraries/lib-lab/table/export"))
            .and(body_json(serde_json::json!({
                "format": "bibtex",
                "query": "",
                "item_type": "",
                "sort": "title",
                "dir": "asc",
                "columns": ["title", "authors", "year", "tags", "doi"],
                "item_ids": ["paper-1"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "filename": "library.bib",
                "media_type": "application/x-bibtex",
                "body": "@article{paper1}\n"
            })))
            .mount(&mock)
            .await;
        let server = Server::new();
        let result = authenticated(
            mock.uri(),
            server.remote_export_bib(&ExportLibraryBibArgs {
                library: "Lab".into(),
                collection: "Reading".into(),
            }),
        )
        .await;
        assert_eq!(result, "@article{paper1}\n");
    }

    #[tokio::test]
    async fn add_by_doi_posts_only_to_the_ridley_save_inbox() {
        let mock = MockServer::start().await;
        mount_ridley_home(&mock).await;
        Mock::given(method("POST"))
            .and(path("/ridley/libraries/lib-lab/inbox"))
            .and(header("authorization", format!("Bearer {KEY}")))
            .and(body_json(serde_json::json!({
                "doi": "10.1000/transformer"
            })))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "queued": true,
                "id": "save-1"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        let server = Server::new();
        let result = authenticated(
            mock.uri(),
            server.remote_add_by_doi(&AddByDoiArgs {
                library: "lib-lab".into(),
                doi: "https://doi.org/10.1000/transformer".into(),
            }),
        )
        .await;
        assert!(result.contains("10.1000/transformer"));
        assert!(result.contains("\"queued\":true"));
    }

    async fn add_by_doi_failure(status: u16, body: serde_json::Value) -> String {
        let mock = MockServer::start().await;
        mount_ridley_home(&mock).await;
        Mock::given(method("POST"))
            .and(path("/ridley/libraries/lib-lab/inbox"))
            .and(header("authorization", format!("Bearer {KEY}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&mock)
            .await;
        let server = Server::new();
        authenticated(
            mock.uri(),
            server.remote_add_by_doi(&AddByDoiArgs {
                library: "lib-lab".into(),
                doi: "10.1000/transformer".into(),
            }),
        )
        .await
    }

    #[tokio::test]
    async fn add_by_doi_404_reports_an_unavailable_save_inbox() {
        let result = add_by_doi_failure(404, serde_json::json!({"error": "route_not_found"})).await;
        assert_eq!(
            result,
            "Ridley save inbox isn't available on this server yet."
        );
    }

    #[tokio::test]
    async fn add_by_doi_404_preserves_explicit_library_not_found() {
        let result =
            add_by_doi_failure(404, serde_json::json!({"error": "library_not_found"})).await;
        assert_eq!(result, "Failed to add DOI: Ridley library not found.");
    }

    #[tokio::test]
    async fn add_by_doi_403_requests_setup_connect_session_bearer() {
        let result = add_by_doi_failure(403, serde_json::json!({"error": "forbidden"})).await;
        assert!(result.contains("OAuth/session bearer"));
        assert!(result.contains("setup --connect"));
    }

    #[tokio::test]
    async fn add_by_doi_5xx_reports_the_dashboard_status() {
        let result =
            add_by_doi_failure(503, serde_json::json!({"error": "temporarily_unavailable"})).await;
        assert!(result.contains("HTTP 503"));
        assert!(result.contains("temporarily_unavailable"));
    }

    #[tokio::test]
    async fn another_users_library_is_not_addressed() {
        let mock = MockServer::start().await;
        mount_home(&mock).await;
        Mock::given(method("GET"))
            .and(path("/libraries/lib-private/table"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;
        let server = Server::new();
        let result = authenticated(
            mock.uri(),
            server.remote_search_library(&SearchLibraryArgs {
                library: "lib-private".into(),
                query: "secret".into(),
                title: None,
                author: None,
                tag: None,
                full_text: false,
            }),
        )
        .await;
        assert!(result.contains("No accessible Ridley library"));
        assert!(!result.contains("secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_adapter_returns_highlights_from_a_fixture_replica_command() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let replica = directory.path().join("library.redb");
        std::fs::write(&replica, b"fixture").unwrap();
        let tt = directory.path().join("tt-fixture");
        std::fs::write(
            &tt,
            "#!/bin/sh\nset -eu\ntest -f \"$TT_RIDLEY_REPLICA\"\ncase \"$*\" in *\"--replica $TT_RIDLEY_REPLICA\"*) ;; *) exit 3 ;; esac\nprintf '%s\\n' '{\"item_id\":\"paper-1\",\"highlights\":[\"scaled dot-product attention\"]}'\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&tt).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&tt, permissions).unwrap();

        let output = run_tt(
            &tt,
            &replica,
            &["library", "get", "paper-1", "--library", "Lab", "--json"],
        )
        .await;
        assert!(output.contains("scaled dot-product attention"));
    }
}
