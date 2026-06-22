//! ookcite-mcp library crate.
//!
//! ## Public surface
//!
//! - [`endpoints`] — HTTP method/path registry; every API call the binary makes
//!   must go through an entry here. `tests/api_contract.rs` validates this
//!   list against the encrypted OpenAPI fixture (`contract/openapi.json.age`).
//!
//! ## Binary layout (not in this lib crate)
//!
//! The `ookcite-mcp` binary lives under `src/` as modules wired from
//! `src/main.rs`:
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | `main` | CLI: `--version`, `setup`, start MCP stdio |
//! | `cli` | Startup probes (API key /me, update check) |
//! | `setup` | `ookcite-mcp setup` / `npx add-mcp` installer |
//! | `server` | `Server` + `#[tool_router]` MCP tools (+ unit tests) |
//! | `tool_args` | MCP tool argument structs (schemars / serde) |
//! | `constants` | `API` base URL, `VERSION`, confidence thresholds |
//! | `http_error` | `error_detail` and status classification helpers |
//! | `collection_entries` | Entry id / DOI alias resolve + search line format |
//! | `resolve_helpers` | Reverse-lookup / free-text resolve payload helpers |
//! | `endpoints` | (this crate) contract-backed endpoint table |
//!
//! Agent-facing behavior (tools, entry_id / DOI remove) is documented in the
//! root `README.md`. Internal maintainability notes live in the TurtleTech
//! obsidian vault (`Software/OokCite/…`) and IssuePR drafts under
//! `Software/IssuePRs/ookcite-mcp/`.
//!
//! ## Why most code is not `pub` here
//!
//! The binary modules share code via `mod` within the binary target. Only
//! `endpoints` is exported so contract tests and helpers (e.g. `setup`) can
//! depend on the registry without pulling in `rmcp` tool macros.

pub mod endpoints;
