# OokCite MCP Server

[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/ookcite-mcp.svg)](https://crates.io/crates/ookcite-mcp)
[![npm](https://img.shields.io/npm/v/@turtletech/ookcite-mcp.svg)](https://www.npmjs.com/package/@turtletech/ookcite-mcp)

Give MCP-capable tools the ability to validate DOIs, format citations, manage
bibliography collections, and catch fabricated references. Returns citation
metadata only -- not PDFs or full-text articles. Works with clients that support
MCP servers over standard input and output.

## Quick Start

One command to install and configure:

```bash
npx @turtletech/ookcite-mcp setup
```

This auto-detects supported MCP clients and writes the configuration for you.
Add an API key for higher rate limits and collection tools:

```bash
npx @turtletech/ookcite-mcp setup --key YOUR_API_KEY
```

No API key required for basic usage (20 lookups/day).
[Sign up](https://my.turtletech.us/signup) for more.

After changing MCP config, restart the client or reload its MCP servers.
Many clients do not hot-reload environment-variable changes for already-running
stdio servers.

## Install (Alternative Methods)

**npm** (recommended):

```bash
npm install -g @turtletech/ookcite-mcp
```

**cargo-binstall** (fastest, no Node.js):

```bash
cargo binstall ookcite-mcp
```

**cargo install** (from source):

```bash
cargo install ookcite-mcp
```

**Pre-built binaries**: Download from
[GitHub Releases](https://github.com/TurtleTech-ehf/ookcite-mcp/releases)
for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows.

## Configure

If you used `setup`, you're done. Otherwise, add to your MCP client config:

```json
{
  "mcpServers": {
    "ookcite": {
      "command": "npx",
      "args": ["-y", "@turtletech/ookcite-mcp"]
    }
  }
}
```

With an API key:

```json
{
  "mcpServers": {
    "ookcite": {
      "command": "npx",
      "args": ["-y", "@turtletech/ookcite-mcp"],
      "env": {
        "OOKCITE_API_KEY": "your_key_here"
      }
    }
  }
}
```

If you installed globally (`npm install -g` or `cargo install`), you can use
`"command": "ookcite-mcp"` directly instead of npx.

To keep the key out of the config file, point `command` at the credential
helper shipped in this package
(`node_modules/@turtletech/ookcite-mcp/scripts/ookcite-mcp-credential-helper`)
and set
`OOKCITE_API_KEY_COMMAND` (for example `pass show services/ookcite-api-key`) or
`OOKCITE_API_KEY_FILE` in `env`. If you write your own wrapper instead, run
every command in it with `</dev/null` and a timeout: a helper that reads stdin
eats the client's `initialize` request, and a secret store waiting on a
passphrase prompt parks the launch past the client's connect timeout. Both
present as the server hanging rather than as a credential problem. The main
README has the detail.

Consult your client's MCP documentation for its configuration-file location.
Use the `mcpServers.ookcite` JSON above when automatic setup is unavailable,
then restart the client or reload its MCP servers.

Optional env (stdio MCP, all clients):

| Variable | Purpose |
| -------- | ------- |
| `OOKCITE_API_KEY` | Higher rate limits + collection tools (optional for basic lookup/format) |
| `OOKCITE_API` | Override API base URL (default `https://ookcite-api.turtletech.us`) |
| `OOKCITE_MCP_READ_ONLY` | `1` hard-disables collection mutations (review / CI automation) |
| `OOKCITE_MCP_ALLOW_MUTATE` | `0` denies mutations; unset or `1` allows (API key still required server-side) |
| `OOKCITE_STARTUP_PROBES` | `1` runs auth + npm update checks on **stderr** before accepting MCP connections (default off for faster connect) |
| `OOKCITE_API_KEY_COMMAND` | Credential helper only: command printing the key on stdout |
| `OOKCITE_API_KEY_FILE` | Credential helper only: file whose first line is the key |
| `OOKCITE_API_KEY_TIMEOUT` | Credential helper only: seconds to allow the lookup (default 10) |

### MCP usage tips

- Prefer **batch** tools (`verify_references`, `batch_format`, `batch_add_to_collection`,
  `import_bibliography`) over many single-citation calls.
- Collection mutations require `OOKCITE_API_KEY`. Destructive tools
  (`delete_collection`, `remove_from_collection`, `unshare_collection`) are
  annotated for clients that honor MCP tool hints.
- The server writes diagnostics to **stderr** only on the MCP path; stdout is
  reserved for JSON-RPC.

## Tools

### Lookup & Validation

| Tool                | Purpose                                       |
| ------------------- | --------------------------------------------- |
| `validate_doi`      | Check if a DOI exists (anti-hallucination)    |
| `lookup_isbn`       | Look up a book by ISBN                        |
| `reverse_lookup`    | Find a paper from messy citation text         |
| `health_check`      | Check API availability and health             |

### Formatting

| Tool                | Purpose                                       |
| ------------------- | --------------------------------------------- |
| `format_citation`   | Format a DOI in any of 2900+ CSL styles       |
| `verify_references` | Batch-check a list of DOIs                    |
| `batch_format`      | Format multiple citations at once             |
| `search_styles`     | Find CSL style IDs by name                    |
| `group_cite`        | Generate grouped in-text markers (e.g. [1-3]) |

### Collections (requires sign-in)

Collections are a signed-in feature. Set `OOKCITE_API_KEY` to use these tools.

| Tool                       | Purpose                                  |
| -------------------------- | ---------------------------------------- |
| `list_collections`         | List saved citation collections          |
| `add_to_collection`        | Add a citation (by DOI or free-text)     |
| `batch_add_to_collection`  | Add multiple citations at once           |
| `import_bibliography`      | Import BibTeX/RIS files into a collection|
| `export_collection`        | Export collection as BibTeX              |
| `search_collection`        | Search within a collection; returns `entry_id` per match |
| `check_duplicates`         | Check for duplicates; returns `entry_id` for matches     |
| `delete_collection`        | Delete a collection                     |
| `update_collection`        | Update name, description, or style      |
| `remove_from_collection`   | Remove an entry by `entry_id`, bare DOI, or `doi:10.x/y` |
| `update_tags`              | Set tags on a collection                |
| `reorder_collection`       | Reorder entries                         |

Typical workflow:

1. Keep `references.bib` or `library.bib` under version control in your project
2. Import that file into an OokCite collection with `import_bibliography`
3. Use `search_collection`, `check_duplicates`, and `export_collection` while revising
4. Treat the collection as an audit/export companion, not the only copy of your bibliography

**Removing a single entry:** call `search_collection` (or `check_duplicates`) to
see each hit as `entry_id: …` (and optionally `aliases: doi:…` when the stored id
is opaque). Pass that `entry_id` to `remove_from_collection`, or pass the paper's
bare DOI / `doi:10.x/y` — the server resolves aliases locally before the API call.

### Sharing & Collection Operations

| Tool                  | Purpose                                    |
| --------------------- | ------------------------------------------ |
| `share_collection`    | Create a shareable link                    |
| `unshare_collection`  | Revoke sharing                             |
| `view_shared`         | View a shared collection by token          |
| `merge_collections`   | Merge multiple collections                 |
| `batch_move_entries`  | Move entries between collections           |

Sharing is available to signed-in accounts with collections.
Free accounts can import and batch-add within their daily quota.
Merge and batch-move require an Academic or Business plan.

## Plans & Pricing

| Tier      | Price     | Lookups/day | API calls/month | Collections | Entries/collection |
| --------- | --------- | ----------- | --------------- | ----------- | ------------------ |
| Anonymous | Free      | 20          | --              | 0           | --                 |
| Free      | Free      | 60          | --              | 4           | 200                |
| Academic  | EUR 4/mo  | 20,000      | 10,000          | 10          | 1,000              |
| Business  | EUR 10/mo | 20,000      | 40,000          | 20          | 4,000              |

Re-lookups can be served without quota use when collection metadata is already
available to the API. A retrieval that has to resolve the paper again can count
against the current plan's quota. Paid Academic checkout is intended for
students, researchers, and educators at accredited institutions; a verified
ORCID can also qualify a signed-in account for Academic limits.

## Anti-Hallucination

Add this to your system prompt:

> Before citing any paper, use validate_doi to confirm the reference exists.
> If validation fails, do not include the citation.

For revision workflows, add:

> Keep the project bibliography in a local `.bib` file under version control.
> Use OokCite collections for verification, deduplication, and export.

## How It Works

The MCP server connects to the public [OokCite](https://ookcite.turtletech.us)
API to look up and format citations. It's a thin MCP wrapper around the OokCite
REST API with no local database, and no heavy dependencies.

[Sign up](https://my.turtletech.us/signup) for a free account (60 lookups/day),
or upgrade to Academic (EUR 4/mo) or Business (EUR 10/mo) for higher monthly
API limits, larger collections, paid utilities, merge, and batch-move.


## Source layout

The crate is a thin MCP (stdio) wrapper around the public OokCite REST API.
There is no local citation database; all state lives on the API.

| Path | Role |
|------|------|
| `src/main.rs` | Binary entry: `--version`, `setup`, start MCP server |
| `src/cli.rs` | Startup probes (validate `OOKCITE_API_KEY` via `/api/v1/me`, update check) |
| `src/setup.rs` | `ookcite-mcp setup` / `npx add-mcp` client config installer |
| `src/server.rs` | `Server` + `#[tool_router]` MCP tool handlers (plus unit tests at bottom) |
| `src/tool_args.rs` | Tool argument structs (`serde` + `schemars`) |
| `src/constants.rs` | API base URL, package version, reverse-lookup confidence threshold |
| `src/http_error.rs` | `error_detail` and HTTP status classification for client-facing strings |
| `src/collection_entries.rs` | Collection entry ids, bare DOI / `doi:` alias resolution, search lines |
| `src/resolve_helpers.rs` | Reverse-lookup and free-text resolve payload helpers |
| `src/endpoints.rs` | Endpoint registry (`lib` crate surface); contract-tested |
| `src/lib.rs` | Library root (exports `endpoints` only) |
| `tests/api_contract.rs` | Decrypts `contract/openapi.json.age`; asserts every endpoint exists |
| `contract/` | Age-encrypted OpenAPI snapshot + `regen.sh` |
| `npm/` | `@turtletech/ookcite-mcp` installer/wrapper (downloads release binary) |
| `demo/` | Asciinema recording scripts |
| `scripts/set-version.sh` | Cocogitto pre-bump hook: `Cargo.toml` + `npm/package.json` version |

**Collections / entry ids:** `search_collection` and `check_duplicates` emit
`entry_id: …` lines. `remove_from_collection` accepts that id, a bare DOI, or
`doi:10.x/y` (resolved locally in `collection_entries` before the DELETE call).

**Release:** tag `v*` runs `.github/workflows/release.yml` (multi-arch GitHub
Release assets, crates.io, npm). Version bumps use [cocogitto](https://docs.cocogitto.io/)
(`cog.toml` + `scripts/set-version.sh`).

**Why `server.rs` is large:** `rmcp`'s `#[tool_router]` / `#[tool]` macros keep
handlers on one `impl Server`. Further file splits without macro workarounds
add little user value; peel tests or add small helpers (`resolve_many`, shared
`Me` type) before fighting the macro.

## Contributing / local checks

```bash
cargo test --bin ookcite-mcp          # unit tests (no contract key needed)
cargo build --release
./target/release/ookcite-mcp --version

# Contract tests (optional locally; required in CI with secret):
export OOKCITE_CONTRACT_KEY=$(pass show turtletech/ookcite-contract-key)
cargo test --test api_contract
```

Live MCP smoke (optional; needs `OOKCITE_API_KEY`): add/search/remove with a
bare DOI on a throwaway collection, then `delete_collection`.

## Documentation

- [Full MCP setup guide](https://ookcite.turtletech.us/docs/howto/mcp-setup/)
- [OokCite app](https://ookcite.turtletech.us/app)
- [TurtleTech](https://turtletech.us)

## License

MIT. see [LICENSE](LICENSE).
