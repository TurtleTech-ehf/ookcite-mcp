# OokCite MCP Server

[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/ookcite-mcp.svg)](https://crates.io/crates/ookcite-mcp)

Give any LLM the ability to validate DOIs, format citations, and catch
hallucinated references. Works with any MCP client: Claude, Codex, Cursor,
Windsurf, OpenCode, Qwen agents, and more.

## Install

**cargo-binstall** (fastest):

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

Add to your MCP client config:

```json
{
  "mcpServers": {
    "ookcite": {
      "command": "ookcite-mcp"
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

No API key required. No Node.js needed. Just the binary.

## Tools

| Tool                | Purpose                                    |
| ------------------- | ------------------------------------------ |
| `validate_doi`      | Check if a DOI exists (anti-hallucination) |
| `lookup_isbn`       | Look up a book by ISBN                     |
| `reverse_lookup`    | Find a paper from messy citation text      |
| `format_citation`   | Format a DOI in any of 2900+ CSL styles    |
| `verify_references` | Batch-check a list of DOIs                 |
| `batch_format`      | Format multiple citations at once          |

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
