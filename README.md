# OokCite MCP Server

[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/ookcite-mcp.svg)](https://crates.io/crates/ookcite-mcp)
[![npm](https://img.shields.io/npm/v/@turtletech/ookcite-mcp.svg)](https://www.npmjs.com/package/@turtletech/ookcite-mcp)

Give any LLM the ability to validate DOIs, format citations, manage
bibliography collections, and catch hallucinated references. Returns citation
metadata only -- not PDFs or full-text articles. Works with any MCP client:
Claude, Codex, Cursor, Windsurf, OpenCode, Qwen agents, and more.

## Quick Start

One command to install and configure:

```bash
npx @turtletech/ookcite-mcp setup
```

This auto-detects your MCP clients (Claude Desktop, Claude Code, Cursor, Codex)
and writes the config for you. Add an API key for higher rate limits and
collection tools:

```bash
npx @turtletech/ookcite-mcp setup --key YOUR_API_KEY
```

No API key required for basic usage (10 lookups/day).
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

Common config file locations:

| Client                 | Config file                                                       |
| ---------------------- | ----------------------------------------------------------------- |
| Claude Desktop (Linux) | `~/.config/Claude/claude_desktop_config.json`                     |
| Claude Desktop (macOS) | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Claude Code            | `.mcp.json` (project) or `~/.claude/settings.json` (global)       |
| Cursor                 | Settings > MCP Servers                                            |
| Codex                  | `~/.codex/config.toml`                                            |

For Codex, the equivalent global registration also works from the CLI:

```bash
codex mcp add ookcite --env OOKCITE_API_KEY=your_key_here -- npx -y @turtletech/ookcite-mcp
```

Then restart Codex so the new stdio server starts with the updated environment.

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

### Sharing & Bulk Operations (requires academic/business plan)

| Tool                  | Purpose                                    |
| --------------------- | ------------------------------------------ |
| `share_collection`    | Create a shareable link                    |
| `unshare_collection`  | Revoke sharing                             |
| `view_shared`         | View a shared collection by token          |
| `merge_collections`   | Merge multiple collections                 |
| `batch_move_entries`  | Move entries between collections           |

## Plans & Pricing

| Tier      | Price   | Lookups/day | Collections | Entries/collection |
| --------- | ------- | ----------- | ----------- | ------------------ |
| Anonymous | Free    | 10          | 0           | --                 |
| Free      | Free    | 30          | 1           | 100                |
| Academic  | $4/mo   | 10,000      | 5           | 500                |
| Business  | $12/mo  | 10,000      | 10           | 2,000              |

Papers in your collections are **free and unlimited** to re-lookup.
Only new lookups count against your daily quota. Papers stay free
as long as they remain in a collection.

Batch operations require an academic or business plan. Academic pricing
is for students, researchers, and educators at accredited institutions.

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

[Sign up](https://my.turtletech.us/signup) for a free account (30 lookups/day),
or upgrade to academic ($4/mo) or business ($12/mo) for batch operations
and larger collections.

## Documentation

- [Full MCP setup guide](https://ookcite.turtletech.us/docs/howto/mcp-setup/)
- [OokCite app](https://ookcite.turtletech.us/app)
- [TurtleTech](https://turtletech.us)

## License

MIT. see [LICENSE](LICENSE).
