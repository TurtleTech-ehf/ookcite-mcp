//! Single source of truth for every HTTP endpoint the MCP server calls.
//!
//! The `tests/api_contract.rs` test validates this list against ttech-cite's
//! OpenAPI spec (encrypted at `contract/openapi.json.age`). Drift between
//! this list and the upstream API causes the test to fail at build time.
//!
//! Tools must construct URLs via [`Endpoint::url`] / [`Endpoint::method`],
//! never via string literals.

#[derive(Debug, Clone, Copy)]
pub struct Endpoint {
    pub method: &'static str,
    /// Path template using OpenAPI `{name}` placeholders.
    pub path: &'static str,
}

impl Endpoint {
    pub const fn new(method: &'static str, path: &'static str) -> Self {
        Self { method, path }
    }

    /// Substitute `{name}` placeholders with URL-encoded values.
    /// Panics if `path` references a placeholder not provided in `params`,
    /// or if `params` has unused entries -- both are programmer errors that
    /// must surface immediately, not at runtime.
    pub fn render(&self, params: &[(&str, &str)]) -> String {
        let mut out = self.path.to_string();
        for (name, value) in params {
            let placeholder = format!("{{{name}}}");
            assert!(
                out.contains(&placeholder),
                "Endpoint {} has no placeholder named {{{}}}; check `endpoints.rs`",
                self.path, name
            );
            out = out.replace(&placeholder, &urlencoding::encode(value));
        }
        assert!(
            !out.contains('{'),
            "Endpoint {} still has unfilled placeholders after substitution: {}",
            self.path, out
        );
        out
    }

    /// Render a full URL by joining `api_base` and the substituted path.
    /// Use this in spawned tasks where a `&Server` is not available.
    pub fn url(&self, api_base: &str, params: &[(&str, &str)]) -> String {
        format!("{api_base}{}", self.render(params))
    }
}

// --- Lookup & validation ---
pub const LOOKUP_DOI: Endpoint = Endpoint::new("POST", "/api/v1/lookup/doi");
pub const LOOKUP_ISBN: Endpoint = Endpoint::new("POST", "/api/v1/lookup/isbn");
pub const REVERSE: Endpoint = Endpoint::new("POST", "/api/v1/reverse");
pub const RESOLVE: Endpoint = Endpoint::new("POST", "/api/v1/resolve");
pub const PARSE_CITATIONS: Endpoint = Endpoint::new("POST", "/api/v1/parse-citations");
pub const RESOLVE_DEBUG: Endpoint = Endpoint::new("POST", "/api/v1/resolve/debug");
pub const HEALTH: Endpoint = Endpoint::new("GET", "/api/health");
pub const ME: Endpoint = Endpoint::new("GET", "/api/v1/me");

// --- Formatting ---
pub const FORMAT: Endpoint = Endpoint::new("POST", "/api/v1/format");
pub const FORMAT_GROUP_CITE: Endpoint = Endpoint::new("POST", "/api/v1/format/group-cite");
pub const STYLES_SEARCH: Endpoint = Endpoint::new("GET", "/api/v1/styles/search");

// --- Collections ---
pub const COLLECTIONS_LIST: Endpoint = Endpoint::new("GET", "/api/v1/collections");
pub const COLLECTIONS_CREATE: Endpoint = Endpoint::new("POST", "/api/v1/collections");
pub const COLLECTION_GET: Endpoint = Endpoint::new("GET", "/api/v1/collections/{id}");
pub const COLLECTION_UPDATE: Endpoint = Endpoint::new("PATCH", "/api/v1/collections/{id}");
pub const COLLECTION_DELETE: Endpoint = Endpoint::new("DELETE", "/api/v1/collections/{id}");
pub const COLLECTION_ENTRIES_ADD: Endpoint = Endpoint::new("POST", "/api/v1/collections/{id}/entries");
pub const COLLECTION_ENTRIES_BATCH: Endpoint = Endpoint::new("POST", "/api/v1/collections/{id}/entries/batch");
pub const COLLECTION_ENTRY_REMOVE: Endpoint = Endpoint::new("DELETE", "/api/v1/collections/{id}/entries/{eid}");
pub const COLLECTION_EXPORT_BIB: Endpoint = Endpoint::new("GET", "/api/v1/collections/{id}/export.bib");
pub const COLLECTION_IMPORT: Endpoint = Endpoint::new("POST", "/api/v1/collections/{id}/import");
pub const COLLECTION_TAGS: Endpoint = Endpoint::new("PATCH", "/api/v1/collections/{id}/tags");
pub const COLLECTION_CHECK_DUPLICATES: Endpoint = Endpoint::new("POST", "/api/v1/collections/{id}/check-duplicates");
pub const COLLECTION_REORDER: Endpoint = Endpoint::new("PATCH", "/api/v1/collections/{id}/reorder");
pub const COLLECTION_SHARE: Endpoint = Endpoint::new("POST", "/api/v1/collections/{id}/share");
pub const COLLECTION_UNSHARE: Endpoint = Endpoint::new("DELETE", "/api/v1/collections/{id}/share");
pub const COLLECTIONS_MERGE: Endpoint = Endpoint::new("POST", "/api/v1/collections/merge");
pub const COLLECTIONS_BATCH_MOVE: Endpoint = Endpoint::new("POST", "/api/v1/collections/batch-move");
pub const SHARED_GET: Endpoint = Endpoint::new("GET", "/api/v1/shared/{token}");

// --- Utilities ---
pub const CITATION_KEYS: Endpoint = Endpoint::new("POST", "/api/v1/citation-keys");
pub const JOURNAL_EXPAND: Endpoint = Endpoint::new("POST", "/api/v1/journal/expand");

/// Every endpoint this crate calls. The contract test iterates over this
/// to verify each one exists in the OpenAPI spec.
pub const ALL: &[Endpoint] = &[
    LOOKUP_DOI, LOOKUP_ISBN, REVERSE, RESOLVE, PARSE_CITATIONS, RESOLVE_DEBUG, HEALTH, ME,
    FORMAT, FORMAT_GROUP_CITE, STYLES_SEARCH,
    COLLECTIONS_LIST, COLLECTIONS_CREATE,
    COLLECTION_GET, COLLECTION_UPDATE, COLLECTION_DELETE,
    COLLECTION_ENTRIES_ADD, COLLECTION_ENTRIES_BATCH, COLLECTION_ENTRY_REMOVE,
    COLLECTION_EXPORT_BIB, COLLECTION_IMPORT, COLLECTION_TAGS,
    COLLECTION_CHECK_DUPLICATES, COLLECTION_REORDER,
    COLLECTION_SHARE, COLLECTION_UNSHARE,
    COLLECTIONS_MERGE, COLLECTIONS_BATCH_MOVE, SHARED_GET,
    CITATION_KEYS, JOURNAL_EXPAND,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_placeholders() {
        assert_eq!(
            COLLECTION_GET.render(&[("id", "abc-123")]),
            "/api/v1/collections/abc-123"
        );
    }

    #[test]
    fn render_url_encodes_values() {
        assert_eq!(
            SHARED_GET.render(&[("token", "a b/c")]),
            "/api/v1/shared/a%20b%2Fc"
        );
    }

    #[test]
    fn render_handles_two_placeholders() {
        assert_eq!(
            COLLECTION_ENTRY_REMOVE.render(&[("id", "col1"), ("eid", "ent2")]),
            "/api/v1/collections/col1/entries/ent2"
        );
    }

    #[test]
    #[should_panic(expected = "no placeholder named")]
    fn render_panics_on_unknown_placeholder() {
        COLLECTION_GET.render(&[("nope", "x")]);
    }

    #[test]
    #[should_panic(expected = "still has unfilled placeholders")]
    fn render_panics_on_missing_substitution() {
        COLLECTION_GET.render(&[]);
    }

    #[test]
    fn all_endpoints_have_unique_method_path_pairs() {
        let mut seen = std::collections::HashSet::new();
        for ep in ALL {
            assert!(
                seen.insert((ep.method, ep.path)),
                "duplicate endpoint in ALL: {} {}",
                ep.method, ep.path
            );
        }
    }
}
