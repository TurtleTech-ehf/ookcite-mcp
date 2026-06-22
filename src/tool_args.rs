//! MCP tool argument types (JSON schema via schemars).

use serde::Deserialize;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DoiArgs {
    /// DOI to validate (e.g. "10.1038/187493a0")
    pub doi: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct IsbnArgs {
    /// ISBN to look up (e.g. "978-0-521-85629-7")
    pub isbn: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReverseArgs {
    /// Free-text search: DOI, ISBN, author name, title, journal, or any combination (e.g. "Goswami JCTC 2026", "Einstein relativity 1905")
    pub text: String,
    /// Optional author name to filter/boost results (e.g. "Goswami")
    #[serde(default)]
    pub author: Option<String>,
    /// Optional journal name to filter/boost results (e.g. "MethodsX", "JCTC")
    #[serde(default)]
    pub journal: Option<String>,
    /// Optional publication year to filter/boost results (e.g. 2026)
    #[serde(default)]
    pub year: Option<i32>,
    /// Optional ORCID ID to filter/boost results by author's ORCID (e.g. "0000-0002-1234-5678")
    #[serde(default)]
    pub orcid: Option<String>,
    /// When true, allow live upstream provider queries in addition to the local corpus.
    #[serde(default)]
    pub use_live_queries: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ParseCitationsArgs {
    /// Raw bibliography text (can contain multiple citations separated by newlines or numbered)
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DebugResolveArgs {
    /// Citation text to debug (free-text query)
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct FormatArgs {
    /// DOI of the paper
    pub doi: String,
    /// CSL style (default: "apa"). Options: apa, ieee, chicago-author-date, mla, nature, vancouver, etc.
    #[serde(default = "default_style")]
    pub style: String,
}
pub fn default_style() -> String {
    "apa".into()
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct VerifyArgs {
    /// List of DOIs to verify exist
    pub dois: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BatchArgs {
    /// Citation strings to resolve (one per entry)
    pub citations: Vec<String>,
    /// CSL style for formatting
    #[serde(default = "default_style")]
    pub style: String,
    /// When true, allow live upstream provider queries in addition to the local corpus.
    #[serde(default)]
    pub use_live_queries: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct StyleSearchArgs {
    /// Query to search for a CSL style (e.g., "american chemical society", "apa", "ieee")
    pub query: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GroupCiteArgs {
    /// List of DOIs to group into a single in-text citation
    pub dois: Vec<String>,
    /// CSL style (default: "apa")
    #[serde(default = "default_style")]
    pub style: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListCollectionsArgs {}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AddToCollectionArgs {
    /// Collection name (creates if doesn't exist)
    pub collection: String,
    /// DOI, ISBN, or free-text search to find the paper to add
    pub query: String,
    /// When true, allow live upstream provider queries in addition to the local corpus.
    #[serde(default)]
    pub use_live_queries: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExportCollectionArgs {
    /// Collection name to export
    pub collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchCollectionArgs {
    /// Collection name to search within
    pub collection: String,
    /// Search query (matches title, author, journal)
    pub query: String,
}

// --- Phase 1: High-value new tools ---

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HealthCheckArgs {}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ImportBibliographyArgs {
    /// Collection name (creates if doesn't exist)
    pub collection: String,
    /// BibTeX or RIS content to import
    pub content: String,
    /// File format: "bibtex" or "ris" (default: "bibtex")
    #[serde(default = "default_bibtex")]
    pub format: String,
}
pub fn default_bibtex() -> String {
    "bibtex".into()
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CheckDuplicatesArgs {
    /// Collection name to check within
    pub collection: String,
    /// DOI or free-text query to check for duplicates
    pub query: String,
    /// When true, allow live upstream provider queries in addition to the local corpus.
    #[serde(default)]
    pub use_live_queries: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BatchAddArgs {
    /// Collection name (creates if doesn't exist)
    pub collection: String,
    /// List of DOIs or free-text queries to resolve and add
    pub queries: Vec<String>,
    /// When true, allow live upstream provider queries in addition to the local corpus.
    #[serde(default)]
    pub use_live_queries: bool,
}

// --- Phase 2: Collection management ---

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeleteCollectionArgs {
    /// Collection name to delete
    pub collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UpdateCollectionArgs {
    /// Current collection name
    pub collection: String,
    /// New name (optional)
    #[serde(default)]
    pub name: Option<String>,
    /// New description (optional)
    #[serde(default)]
    pub description: Option<String>,
    /// New default CSL style (optional)
    #[serde(default)]
    pub default_style: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RemoveFromCollectionArgs {
    /// Collection name
    pub collection: String,
    /// Entry to remove: opaque entry id from search_collection, `doi:10.x/y`, or bare DOI
    pub entry_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UpdateTagsArgs {
    /// Collection name
    pub collection: String,
    /// New tag list (replaces existing tags)
    pub tags: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReorderCollectionArgs {
    /// Collection name
    pub collection: String,
    /// Ordered list of entry IDs
    pub entry_ids: Vec<String>,
}

// --- Phase 3: Sharing and bulk ops ---

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ShareCollectionArgs {
    /// Collection name to share
    pub collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UnshareCollectionArgs {
    /// Collection name to unshare
    pub collection: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MergeCollectionsArgs {
    /// Collection names to merge (first becomes target)
    pub collections: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BatchMoveArgs {
    /// Source collection name
    pub source: String,
    /// Target collection name
    pub target: String,
    /// Entry IDs to move
    pub entry_ids: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ViewSharedArgs {
    /// Share token from a shared collection URL
    pub share_token: String,
}

// --- New utility tools ---

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GenerateCitationKeysArgs {
    /// List of DOIs to generate citation keys for
    pub dois: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExpandJournalArgs {
    /// Journal abbreviation to expand (e.g. "JACS", "J. Am. Chem. Soc.")
    pub abbreviation: String,
}

