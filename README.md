# OokCite MCP Server

[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/ookcite-mcp.svg)](https://crates.io/crates/ookcite-mcp)
[![npm](https://img.shields.io/npm/v/@turtletech/ookcite-mcp.svg)](https://www.npmjs.com/package/@turtletech/ookcite-mcp)

Give any LLM the ability to validate DOIs, format citations, and catch
hallucinated references. Works with any MCP client: Claude, Codex, Cursor,
Windsurf, OpenCode, Qwen agents, and more.

## Quick Start

One command to install and configure:

```bash
npx @turtletech/ookcite-mcp setup
```

This auto-detects your MCP clients (Claude Desktop, Claude Code, Cursor, Codex)
and writes the config for you. Add an API key for higher rate limits:

```bash
npx @turtletech/ookcite-mcp setup --key YOUR_API_KEY
```

No API key required for basic usage (10 lookups/day). [Sign up](https://my.turtletech.us/signup) for more.

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
      "command": "ookcite-mcp"
    }
  }
}
```

With an API key:

```json
{
  "mcpServers": {
    "ookcite": {
      "command": "ookcite-mcp",
      "env": {
        "OOKCITE_API_KEY": "your_key_here"
      }
    }
  }
}
```

Common config file locations:

| Client                 | Config file                                                       |
| ---------------------- | ----------------------------------------------------------------- |
| Claude Desktop (Linux) | `~/.config/Claude/claude_desktop_config.json`                     |
| Claude Desktop (macOS) | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Claude Code            | `.mcp.json` (project) or `~/.claude/settings.json` (global)       |
| Cursor                 | Settings > MCP Servers                                            |
| Codex                  | `~/.codex/config.json`                                            |

## Tools

| Tool                | Purpose                                       |
| ------------------- | --------------------------------------------- |
| `validate_doi`      | Check if a DOI exists (anti-hallucination)    |
| `lookup_isbn`       | Look up a book by ISBN                        |
| `reverse_lookup`    | Find a paper from messy citation text         |
| `format_citation`   | Format a DOI in any of 2900+ CSL styles       |
| `verify_references` | Batch-check a list of DOIs                    |
| `batch_format`      | Format multiple citations at once             |
| `search_styles`     | Find CSL style IDs by name                    |
| `group_cite`        | Generate grouped in-text markers (e.g. [1-3]) |

## Anti-Hallucination

Add this to your system prompt:

> Before citing any paper, use validate_doi to confirm the reference exists.
> If validation fails, do not include the citation.

## How It Works

The MCP server connects to the public [OokCite](https://ookcite.turtletech.us)
API to look up and format citations. It's a thin MCP wrapper around the OokCite
REST API with no local database, and no heavy dependencies.

Rate limits apply: 10 lookups/day anonymous, 30/day with a free account. [Sign up](https://my.turtletech.us/signup) for more, or upgrade (starting at $4/month)
for more.

## Documentation

- [Full MCP setup guide](https://ookcite.turtletech.us/docs/howto/mcp-setup/)
- [OokCite app](https://ookcite.turtletech.us/app)
- [TurtleTech](https://turtletech.us)

## License

MIT. see [LICENSE](LICENSE).
