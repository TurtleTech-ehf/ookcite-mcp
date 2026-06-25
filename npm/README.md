# OokCite MCP Server

[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/ookcite-mcp.svg)](https://crates.io/crates/ookcite-mcp)
[![npm](https://img.shields.io/npm/v/@turtletech/ookcite-mcp.svg)](https://www.npmjs.com/package/@turtletech/ookcite-mcp)

Give any LLM the ability to validate DOIs, format citations, manage
bibliography collections, and catch hallucinated references. Works with any
MCP client: Grok Build, Claude, Codex, Cursor, Windsurf, OpenCode, Qwen agents,
and more.

## Quick Start

One command to install and configure:

```bash
npx @turtletech/ookcite-mcp setup
```

This auto-detects supported MCP clients (Claude Desktop, Claude Code, Cursor,
Codex) and writes the config for you. Grok Build is not covered by `setup` —
install via the plugin marketplace or the Grok config in the main
[README](https://github.com/TurtleTech-ehf/ookcite-mcp#grok-build). Add an API
key for higher rate limits and collection tools:

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
| Grok Build             | Plugin install or project `.mcp.json` (see main README)           |
| Claude Desktop (Linux) | `~/.config/Claude/claude_desktop_config.json`                     |
| Claude Desktop (macOS) | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Claude Code            | `.mcp.json` (project) or `~/.claude/settings.json` (global)       |
| Cursor                 | Settings > MCP Servers                                            |
| Codex                  | `~/.codex/config.toml`                                            |

### Grok Build

Install from the xAI plugin marketplace (`/marketplace`, search `ookcite`) once
published, or point Grok at this package's upstream repo plugin files
(`.mcp.json` + `plugin.json` at the repo root). Set `OOKCITE_API_KEY` for
collection tools. Full steps:
[Grok Build section in the main README](https://github.com/TurtleTech-ehf/ookcite-mcp#grok-build).

For Codex, the equivalent global registration also works from the CLI:

```bash
codex mcp add ookcite --env OOKCITE_API_KEY=your_key_here -- npx -y @turtletech/ookcite-mcp
```

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

| Tool                      | Purpose                                  |
| ------------------------- | ---------------------------------------- |
| `list_collections`        | List saved citation collections          |
| `add_to_collection`       | Add a citation (by DOI or free-text)     |
| `batch_add_to_collection` | Add multiple citations at once           |
| `import_bibliography`     | Import BibTeX/RIS files into a collection|
| `export_collection`       | Export collection as BibTeX              |
| `search_collection`       | Search within a collection; returns `entry_id` per match |
| `check_duplicates`        | Check for duplicates; returns `entry_id` for matches     |
| `remove_from_collection`  | Remove by `entry_id`, bare DOI, or `doi:10.x/y`          |

Typical workflow:

1. Keep `references.bib` or `library.bib` under version control in your project
2. Import that file into an OokCite collection with `import_bibliography`
3. Use the collection to audit, deduplicate, and export while revising
4. Keep the local `.bib` file as the source-controlled canonical bibliography

To delete one paper from a collection, use `search_collection` (or
`check_duplicates`) for `entry_id: …`, then `remove_from_collection` with that
id — or pass a bare DOI / `doi:10.x/y` and let the server resolve it.

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

Rate limits apply: 10 lookups/day anonymous, 30/day with a free account.
[Sign up](https://my.turtletech.us/signup) for more, or upgrade (starting at
$4/month) for more.

## Documentation

- [Full MCP setup guide](https://ookcite.turtletech.us/docs/howto/mcp-setup/)
- [OokCite app](https://ookcite.turtletech.us/app)
- [TurtleTech](https://turtletech.us)

## License

MIT. see [LICENSE](LICENSE).
