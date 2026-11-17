# OokCite MCP Server

[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/ookcite-mcp.svg)](https://crates.io/crates/ookcite-mcp)
[![npm](https://img.shields.io/npm/v/@turtletech/ookcite-mcp.svg)](https://www.npmjs.com/package/@turtletech/ookcite-mcp)

Give MCP-capable tools the ability to validate DOIs, format citations, manage
bibliography collections, and catch fabricated references. Returns citation
metadata only -- not PDFs or full-text articles. `find_open_access_pdf` returns
a link to a legal open-access copy; the server never downloads a file. Desktop clients can run it
over standard input and output. Remote clients use
`https://ookcite-api.turtletech.us/mcp`.

## Quick Start

One command to install and configure:

```bash
npx @turtletech/ookcite-mcp setup
```

This auto-detects supported MCP clients and writes the configuration for you.
Connect an OokCite account for higher rate limits and collection tools without
putting an API key in shell arguments or MCP configuration:

```bash
npx @turtletech/ookcite-mcp setup --connect
```

The command opens the TurtleTech dashboard, creates or activates the Free plan,
stores the issued key in the platform credential store, and writes only a
credential reference to detected clients. Use `setup --connect --device` on a
headless machine.

An existing API key remains supported:

```bash
npx @turtletech/ookcite-mcp setup --key YOUR_API_KEY
```

No API key required for basic usage (40 lookups/day): `format_citation` and a
small plaintext `import_bibliography` (omit `collection`, up to 40 items).
[Sign up](https://my.turtletech.us/signup?service=ookcite&source=ookcite_mcp) for more.

### Paste a list to BibTeX

`import_bibliography` accepts a numbered or blank-line citation list, not only
`.bib` / `.ris`. Omit `collection` for a small paste: the tool resolves the
items and returns `.bib` text. With a key, pass `collection` to save the
resolved entries. `export_collection` can then emit `.bib` (`format=bib`) or
CSL bibliography text (`format=csl` or a style id such as `ieee`).
`npx @turtletech/ookcite-mcp setup` remains the installer. The server never
fetches PDFs.

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

### Keeping the key out of the config file

`setup --connect` uses the platform credential store by default. It refuses to
replace an existing platform credential or named OokCite MCP configuration
unless `--replace-credential` or `--replace-config` is given explicitly.

A generic credential manager can be used instead. The store command receives
the new key on standard input; it must not expect the key in a command-line
argument. The retrieval command prints the key on standard output when the MCP
server starts:

```bash
npx @turtletech/ookcite-mcp setup --connect \
  --store-command "credential-cli store ookcite" \
  --retrieve-command "credential-cli read ookcite"
```

For an explicit owner-only file instead of a credential manager:

```bash
npx @turtletech/ookcite-mcp setup --connect \
  --credential-file "$HOME/.config/ookcite/api-key"
```

The file is created with owner-only permissions and is never overwritten. The
platform, helper-command, and file forms put references such as these in client
configuration:

```json
{
  "mcpServers": {
    "ookcite": {
      "command": "npx",
      "args": ["-y", "@turtletech/ookcite-mcp"],
      "env": {
        "OOKCITE_API_KEY_COMMAND": "credential-cli read ookcite"
      }
    }
  }
}
```

At startup, source precedence remains `OOKCITE_API_KEY`,
`OOKCITE_API_KEY_COMMAND`, `OOKCITE_API_KEY_FILE`, then the platform credential
reference. With none of them the server starts anonymous. `setup --key` remains
available for existing deployments that intentionally keep the key in client
configuration.

Credential retrieval obeys two constraints:

- Retrieval receives closed standard input, so it cannot consume MCP JSON-RPC.
- Retrieval is bounded by `OOKCITE_API_KEY_TIMEOUT`, which defaults to 10
  seconds, and its output is never copied into diagnostics.

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
| `OOKCITE_API_KEY_COMMAND` | Command printing the key on stdout; stdin is closed |
| `OOKCITE_API_KEY_FILE` | Owner-protected file whose first line is the key |
| `OOKCITE_API_KEY_TIMEOUT` | Seconds allowed for credential retrieval (default 10) |
| `OOKCITE_CREDENTIAL_STORE` | `platform` to load a platform credential reference |
| `OOKCITE_CREDENTIAL_SERVICE` | Platform credential service name (default `ookcite-mcp`) |
| `OOKCITE_CREDENTIAL_ACCOUNT` | Platform credential account name (default `default`) |
| `OOKCITE_RIDLEY_MODE` | Ridley read mode: `local` (default) or metadata-only `remote` |
| `OOKCITE_RIDLEY_REPLICA` | Path to the local ridley-core replica; required in local mode |
| `OOKCITE_RIDLEY_TT` | Path to `tt` when it is not on `PATH` |

## Marketplace plugin

OokCite installs two ways.

From this repository, add marketplace `TurtleTech-ehf/ookcite-mcp` and
install `ookcite@ookcite`. That registers the `ookcite` marketplace and
enables the plugin in `plugins/ookcite`. The plugin connects to
`https://ookcite-api.turtletech.us/mcp` and adds four skills: format a
pasted bibliography (`/ookcite:format-bibliography`), verify references
before submission (`/ookcite:verify-references`), audit a manuscript `.bib`
for broken or retracted entries (`/ookcite:audit-bibliography`), and build
and export a collection (`/ookcite:build-collection`). Sign in when asked. Formatting
one citation and checking a short DOI list also work before sign-in, within
the anonymous daily cap. Saving a collection requires the signed-in account.

The web, desktop, and cowork apps use the same address as a directory
connector. Choose sign-in. Submit the connector by that URL, and submit the
`plugins/ookcite` folder as the plugin bundle. Installing from the
marketplace does not submit either listing.

A remote connector does not have to support dynamic client registration.
The plugin sends the pre-registered public client id `ookcite-remote`, so
the code client does not call a registration endpoint. Discovery for that
client publishes S256 PKCE and no `registration_endpoint`. The protected
resource lists that client's issuer first, and its `resource` field is
`https://ookcite-api.turtletech.us/mcp`. The client accepts the directory
callback exactly, and loopback redirects on `localhost` and `127.0.0.1` at
any port. Access tokens must satisfy `exp - iat` <= 3600 or the resource
server rejects them.

## Remote MCP

Use OokCite from a chat product without installing anything. Add this address, choose sign-in, and approve the login when asked:

```text
https://ookcite-api.turtletech.us/mcp
```

The chat then sends a short-lived access token:

```text
Authorization: Bearer <access token>
```

Lookups count against that signed-in account. A pasted API key is rejected. The transport is stateless Streamable HTTP: one POST, JSON in and JSON out. Select Streamable HTTP and sign-in (OAuth). Do not select the legacy SSE transport, an API key, or "No authentication".

eduGenAI Chat, after `ookcite-api.turtletech.us` is whitelisted:

| Field | Value |
| ----- | ----- |
| Name | OokCite |
| Description | Verify DOIs and ISBNs, resolve messy citations, and format bibliography entries in CSL styles. Returns citation metadata only, not PDFs or full text. |
| URL | `https://ookcite-api.turtletech.us/mcp` |
| Transport | Streamable HTTP |
| Authentication | OAuth |

`ookcite-mcp serve` runs your own copy. Desktop clients that spawn a subprocess still use standard input and output.

| Variable | Purpose |
| -------- | ------- |
| `OOKCITE_API` | OokCite API base URL (default `https://ookcite-api.turtletech.us`) |
| `OOKCITE_MCP_HTTP_AUTH` | `oauth` for sign-in, `bearer` for your own copy, or `none` |
| `OOKCITE_MCP_OIDC_ISSUER` | Sign-in issuer. Required when auth is `oauth` |
| `OOKCITE_MCP_OIDC_AUDIENCE` | Token audience. Required when auth is `oauth` |
| `OOKCITE_MCP_OIDC_SCOPE` | Required scope (default `openid`) |
| `OOKCITE_MCP_PATH` | MCP path (default `/mcp`) |
| `OOKCITE_MCP_ALLOWED_HOSTS` | Extra `Host` names, comma-separated. Loopback is always allowed |
| `OOKCITE_MCP_ALLOWED_ORIGINS` | Browser `Origin` values that may call the endpoint. A missing Origin is allowed |

### MCP usage tips

- Prefer **batch** tools (`verify_references`, `audit_bibliography`, `batch_format`, `batch_add_to_collection`,
  `import_bibliography`) over many single-citation calls. A manuscript `.bib` goes to
  `audit_bibliography`; a pasted list that is not BibTeX goes to `import_bibliography`.
- Collection mutations require the signed-in account, or `OOKCITE_API_KEY` on a
  copy you run yourself. Destructive tools
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
| `reverse_lookup`    | Find a paper from messy citation text; returns original query, confidence, and title |
| `batch_resolve`     | Resolve many citation strings in one request (max 50) |
| `enhanced_search`   | Corpus search with author / category / citation facets |
| `health_check`      | Check API availability and health             |

### Formatting

| Tool                | Purpose                                       |
| ------------------- | --------------------------------------------- |
| `format_citation`   | Format a DOI in any of 2900+ CSL styles (no API key for a single citation) |
| `verify_references` | Batch-check DOIs, and any bibliographic claim passed with them |
| `audit_bibliography` | Audit a manuscript `.bib` for broken DOIs and publisher retractions |
| `batch_format`      | Format multiple citations at once             |
| `search_styles`     | Find CSL style IDs by name                    |
| `list_styles`       | Page through the full CSL style list          |
| `group_cite`        | Generate grouped in-text markers (e.g. [1-3]) |

### Discovery

| Tool                   | Purpose                                                 |
| ---------------------- | ------------------------------------------------------- |
| `find_open_access_pdf` | Link to a legal open-access copy of a DOI (Unpaywall)   |
| `related_papers`       | Related works, references, or citing works of a DOI (OpenAlex) |
| `cite_while_writing`   | Candidate citations for one claim sentence, with reasons |
| `check_bibliography`   | Pasted BibTeX or RIS: DOIs, retractions, preprint upgrades, duplicates |

See [Discovery tools](#discovery-tools) for inputs, errors, and example calls.

### Account

| Tool     | Purpose                                                  |
| -------- | -------------------------------------------------------- |
| `usage`  | Plan in effect plus daily (and monthly) lookups remaining |

### ORCID

| Tool             | Purpose                                                |
| ---------------- | ------------------------------------------------------ |
| `orcid_search`   | Find ORCID profiles by name, affiliation, or ORCID ID  |
| `orcid_profile`  | Fetch one ORCID profile by ID                          |
| `ingest_orcid`   | Index an ORCID profile's publications so they are searchable |

### Ridley library

Ridley reads are local by default because the dashboard cannot open sealed
library objects. Configure `OOKCITE_RIDLEY_REPLICA`; the server delegates to
`tt library`, which opens the replica through ridley-core. The signed-in
credential from `setup --connect` is reused for library discovery and the
save inbox.

| Tool               | Purpose |
| ------------------ | ------- |
| `list_libraries`   | List visible Ridley libraries |
| `search_library`   | Search title, author, tag, notes, highlights and full text |
| `get_item`         | Return metadata, notes and highlights |
| `add_by_doi`       | Queue a DOI in the Ridley save inbox |
| `export_bib`       | Export one local Ridley collection as BibTeX |

Set `OOKCITE_RIDLEY_MODE=remote` only for the dashboard's metadata-only
catalogue view. See [Ridley library tools](docs/ridley-library.md) for the
security boundary and both mode configurations.

### Collections (requires sign-in)

Collections are a signed-in feature. The hosted server uses the account from
sign-in. A copy you run yourself uses `OOKCITE_API_KEY`. A small plaintext
`import_bibliography` with no `collection` is the exception: it returns `.bib`
under the anonymous cap.

| Tool                       | Purpose                                  |
| -------------------------- | ---------------------------------------- |
| `list_collections`         | List saved citation collections          |
| `add_to_collection`        | Add a citation (by DOI or free-text)     |
| `batch_add_to_collection`  | Add multiple citations at once           |
| `import_bibliography`      | Import BibTeX, RIS, or a pasted plaintext list |
| `export_collection`        | Export collection as BibTeX or CSL text  |
| `search_collection`        | Search within a collection; returns `entry_id` per match |
| `check_duplicates`         | Check for duplicates; returns `entry_id` for matches     |
| `delete_collection`        | Delete a collection                     |
| `update_collection`        | Update name, description, or style      |
| `remove_from_collection`   | Remove an entry by `entry_id`, bare DOI, or `doi:10.x/y` |
| `update_entry_metadata`    | Correct a saved entry's title, authors, year, DOI, … |
| `merge_entries`            | Merge two entries of one collection into one |
| `update_tags`              | Set tags on a collection                |
| `reorder_collection`       | Reorder entries                         |
| `save_to_ridley`           | Queue a paper for the user's Ridley library |

Typical workflow:

1. Paste a numbered citation list into `import_bibliography` (omit `collection` for a small list) and keep the returned `.bib`
2. Or keep `references.bib` / `library.bib` under version control and import that file into a collection
3. Use `search_collection`, `check_duplicates`, and `export_collection` (`bib` or `csl`) while revising
4. Treat the collection as an audit/export companion, not the only copy of your bibliography

**Local overrides:** the API does not yet store every field
`update_entry_metadata` sends, so a copy running on your own machine (stdio, or
`serve` with `auth=none`) also keeps each edit in
`$XDG_DATA_HOME/ookcite/overrides/<collection>.json` and applies it in
`export_collection`. A signed-in HTTP server keeps no local overrides.
`eid_as=pages` places article numbers where elsarticle-num prints them. See
[`docs/local-overrides.md`](docs/local-overrides.md).

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

### Utilities

| Tool                      | Purpose                                       |
| ------------------------- | --------------------------------------------- |
| `generate_citation_keys`  | Better BibTeX-style keys for a list of DOIs   |
| `expand_journal`          | Expand a journal abbreviation to its full name |
| `normalize_bibliography`  | Re-render BibTeX or RIS as canonical BibTeX   |

These three require an Academic or Business plan.

## Discovery tools

`find_open_access_pdf` and `related_papers` read two public indexes next to the
OokCite API: Unpaywall for open-access locations and OpenAlex for the citation
graph. They return links and metadata, never files. Those calls go out over
https to those two hosts only, with redirects pinned to the same hosts, and a response is read to at most
1 MiB. They send a contact address, `rgoswami@ieee.org` by default, because
both services ask for one; set `OOKCITE_MCP_CONTACT_EMAIL` to use your own.

Every tool takes a closed JSON object: an unknown field is refused, and each
field carries its type, length, range, or pattern in the tool schema.

Errors keep their identity. A `402 Payment Required` or `429 Too Many
Requests` from a provider or from OokCite comes back as an error result that
starts with `PAYMENT REQUIRED` or `RATE LIMITED`, carries the `Retry-After`
value the server sent, and sets `structuredContent.http_status` and
`structuredContent.retry_after`. A throttle is never reported as "not found".

### find_open_access_pdf

Input: `{"doi": "<bare DOI, doi: form, or doi.org URL>"}`.

Only locations Unpaywall records as open access are returned, each with its
host (publisher or repository), version (published, accepted, or submitted),
and licence. `NOT OPEN ACCESS` means no free legal copy is recorded; it is an
answer, not an error. The tool does not query shadow libraries.

Example (mock data):

```
> find_open_access_pdf {"doi": "10.5555/demo.oa.2024"}
OPEN ACCESS doi:10.5555/demo.oa.2024
Title: A Mock Study of Open Access Links
Venue: Journal of Mock Results (2024)
OA status: gold
Best copy (PDF): https://journals.example.org/article/demo.oa.2024.pdf
Landing page: https://journals.example.org/article/demo.oa.2024
  host: publisher | version: published version | licence: cc-by
Other legal copies (1):
  - page https://repo.example.edu/handle/demo/2024 (host: repository (Example University) | version: published version | licence: not stated)
Source: Unpaywall. Links only: no file was downloaded, and no shadow library was consulted.
```

A throttled call:

```
> find_open_access_pdf {"doi": "10.5555/demo.oa.2024"}
RATE LIMITED Unpaywall : 429 Too Many Requests: Too many requests (Retry-After: 120)
This limit belongs to Unpaywall, not to OokCite. Wait for Retry-After before calling again.
```

### related_papers

Input: `{"doi": "...", "relation": "related" | "references" | "cited_by", "limit": 1-25}`.
`relation` defaults to `related` and `limit` to 10.

`related` is OpenAlex's own similarity ranking, so treat it as a list of leads
to read. `references` is what the paper cites, and `cited_by` is what cites it,
most cited first. Each result has the title, year, first authors, venue, DOI,
and citation count. It is metadata only: run `validate_doi` on a DOI before you
cite it. If you have an OpenAlex key, set `OPENALEX_API_KEY` and it is sent
with each request.

Example (mock data):

```
> related_papers {"doi": "10.5555/demo.seed", "relation": "related", "limit": 2}
RELATED WORKS for doi:10.5555/demo.seed (source: OpenAlex)
Seed: A Mock Seed Paper (2021) | cited by 120
1. First Related Study (2020) -- Ada Example, Ben Sample, Chi Mock et al. | Journal of Mock Results | doi:10.5555/demo.rel1 | cited by 41
2. Second Related Study (2019) -- Ada Example, Ben Sample, Chi Mock et al. | Journal of Mock Results | doi:10.5555/demo.rel2 | cited by 8
Showing 2 of 3 related works.
OpenAlex ranks related works by shared concepts and citations. Treat them as leads to read, not as papers that agree with the seed.
Metadata only. Check a DOI with validate_doi before citing it, and format it with format_citation.
```

### cite_while_writing

Input: `{"claim": "<sentence>", "limit": 1-10, "year_from": 1000-2100, "check_retractions": true}`.
Only `claim` is required (12 to 2000 characters). `limit` defaults to 5 and
`check_retractions` to true.

The claim is cut to its content words and searched in the OokCite corpus. Each
candidate lists the claim words its title and venue share, its corpus score,
year, venue, and DOI, so the reason for each suggestion is something you can
read, not a verdict. A candidate whose Crossref or DataCite record carries a
retraction, withdrawal, or expression of concern is held back and named
(`HELD BACK doi:... retracted by doi:...`), and the next hit takes its place.
If the publisher record cannot be read, the candidate is shown as `UNCHECKED`
rather than called clean. Matching is by keyword: no candidate is evidence that
the paper supports the claim, so read it before citing, then use
`validate_doi` and `format_citation` on the one you pick.

Example (mock data):

```
> cite_while_writing {"claim": "Sheared colloidal glasses nucleate crystals faster than quiescent suspensions do.", "limit": 2}
CITATION CANDIDATES for: "Sheared colloidal glasses nucleate crystals faster than quiescent suspensions do."
Searched the OokCite corpus for: sheared colloidal glasses nucleate crystals faster quiescent suspensions
1. Crystal nucleation in sheared colloidal glasses (2021) | Journal of Mock Physics | doi:10.5555/demo.a
   Why: title and venue share 5 of 8 claim words (sheared, colloidal, glasses, nucleate, crystals), title match high; corpus score 40
   Notices: no retraction, withdrawal, or concern in the publisher record
2. Quiescent suspensions of hard spheres (2015) | Soft Matter Demo | doi:10.5555/demo.b
   Why: title and venue share 2 of 8 claim words (quiescent, suspensions), title match medium; corpus score 90
   Notices: no retraction, withdrawal, or concern in the publisher record
Left out: 1 hit(s) with no DOI, 0 older than year_from.
These are keyword matches on titles and the corpus index. None is evidence that the paper supports the claim: read it, then validate_doi and format_citation the one you use.
```

### check_bibliography

Input: `{"content": "<BibTeX or RIS text>", "format": "auto" | "bibtex" | "ris", "resolve_missing_dois": true}`.
Only `content` is required (up to 1 MiB and 2000 entries). `format` defaults to
`auto`, which reads RIS when the text has `TY  -` records.

For each entry it reports the DOI (the one written, or one found from the title
when the entry has none), a retraction, withdrawal, or expression of concern
from the Crossref or DataCite record, and, for a preprint, the published
version the publisher records (`UPGRADE`). A DOI found from a title is accepted
only when the title agrees closely and neither the year nor the first author
conflicts; otherwise the entry stays `NO DOI` and the closest record is named,
not used. Duplicates are grouped by DOI, by title (when year and first author
do not conflict), and as a preprint next to its published version. Set
`resolve_missing_dois` to false to skip title lookups.

Each DOI looked up counts against the daily quota. A title lookup counts three
times: resolve, reverse, and the lookup of the DOI they find. A paste that
cannot fit is refused before any request. If a provider answers `429` or `402`
for an entry from the publisher record, that entry is
`UPSTREAM` with the provider's `Retry-After`, never `OK` or `UNCHECKED`, and
the result is an error result as described above. This tool reads text you
paste and checks the DOIs. For a manuscript `.bib` compared field by field with
the record, use `audit_bibliography`. RIS is read here only.

Example (mock data):

```
> check_bibliography {"content": "@article{clean2020, ...}\n... (8 entries)"}
BIBLIOGRAPHY CHECK (BibTeX): 8 entries, 6 with a DOI, 1 DOI(s) found from titles
Summary: 1 RETRACTED, 2 UPGRADE, 1 NO DOI, 4 OK; 2 duplicate group(s)
[clean2020] OK doi:10.5555/clean.2020 (article)
[clean2020b] OK doi:10.5555/clean.2020 (article)
[retracted2019] RETRACTED doi:10.5555/retracted.2019 (article)
    retracted by doi:10.5555/notice.2020 on 2020-06
[preprint2021] UPGRADE doi:10.5555/preprint.2021 (misc)
    preprint (Crossref type posted-content); published version doi:10.5555/pub.2021. Cite the published version.
[published2021] OK doi:10.5555/pub.2021 (article)
[arxiv2022] UPGRADE doi:10.48550/arxiv.2201.00001 (misc)
    preprint (DataCite resource type Preprint); published version doi:10.5555/pub.2022. Cite the published version.
[nodoi2018] OK doi:10.5555/found.2018 (article)
    DOI found from the title: doi:10.5555/found.2018 (title overlap 100%, year and first author do not conflict). Add it to the entry.
[ghost2017] NO DOI (article)
    no DOI; no record matches the title
Duplicates:
- [clean2020], [clean2020b]: same DOI doi:10.5555/clean.2020
- [preprint2021], [published2021]: preprint doi:10.5555/preprint.2021 and its published version doi:10.5555/pub.2021
Metadata only. This does not compare titles, authors, or pages with the record: run audit_bibliography on the corrected .bib for that.
```

### Try the new tools without a network

`demo/mock_upstream.py` is one plain-http mock for OokCite, Crossref, DataCite,
Unpaywall and OpenAlex. A build with the off-by-default `demo-mock` feature
sends all of its calls to the host named in `OOKCITE_DEMO_MOCK_BASE`; a normal
build ignores that variable and the feature is never part of a release.
`demo/mcp_tools_demo.py` starts both, speaks MCP over stdio, and calls each new
tool once:

```
cargo build --features demo-mock
demo/mcp_tools_demo.py                       # all four tools
demo/mcp_tools_demo.py --tool check_bibliography | demo/ansi_to_png.py out.png
```

## Plans & Pricing

| Tier      | Price     | Lookups/day | API calls/month | Collections | Entries/collection |
| --------- | --------- | ----------- | --------------- | ----------- | ------------------ |
| Anonymous | Free      | 40          | --              | 0           | --                 |
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
>
> Before submission, run audit_bibliography on the manuscript `.bib`. A DOI
> that resolves can still be retracted, withdrawn, or a different paper.

For revision workflows, add:

> Keep the project bibliography in a local `.bib` file under version control.
> Use OokCite collections for verification, deduplication, and export.

## How It Works

The MCP server connects to the public [OokCite](https://ookcite.turtletech.us)
API to look up and format citations. It's a thin MCP wrapper around the OokCite
REST API with no local database, and no heavy dependencies.

[Sign up](https://my.turtletech.us/signup?service=ookcite&source=ookcite_mcp) for a free account (60 lookups/day),
or upgrade to Academic (EUR 4/mo) or Business (EUR 10/mo) for higher monthly
API limits, larger collections, paid utilities, merge, and batch-move.


## Source layout

The crate is a thin MCP (stdio) wrapper around the public OokCite REST API.
There is no local citation database; all state lives on the API, except the
per-collection metadata overrides described in `docs/local-overrides.md`.

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
| `src/plaintext.rs` | Pasted-list split and local `.bib` render |
| `src/bib_audit.rs` | Manuscript `.bib` parse, publisher retraction notices, and the audit report |
| `src/bib_export.rs` | Collection BibTeX from provider records, stored fields, and text cleanup |
| `src/overrides.rs` | Local per-collection metadata overrides applied on export |
| `src/resolve_helpers.rs` | Reverse-lookup and free-text resolve payload helpers |
| `src/endpoints.rs` | Endpoint registry (`lib` crate surface); contract-tested |
| `src/lib.rs` | Library root (exports `endpoints` only) |
| `tests/api_contract.rs` | Decrypts `contract/openapi.json.age`; asserts every endpoint exists |
| `contract/` | Age-encrypted OpenAPI snapshot + `regen.sh` |
| `npm/` | `@turtletech/ookcite-mcp` installer/wrapper (downloads release binary) |
| `demo/` | Asciinema recording scripts |
| marketplace catalog | Lists `ookcite` from `./plugins/ookcite` |
| `plugins/ookcite/` | Plugin manifest, hosted MCP URL, four skills |
| `scripts/set-version.sh` | Cocogitto pre-bump hook: package, registry, and plugin versions |

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
export OOKCITE_CONTRACT_KEY="value-from-your-credential-manager"
cargo test --test api_contract
```

Live MCP smoke (optional; needs `OOKCITE_API_KEY`): add/search/remove with a
bare DOI on a throwaway collection, then `delete_collection`.

## Documentation

- [Audit a manuscript bibliography](docs/audit-bibliography.md)
- [Full MCP setup guide](https://ookcite.turtletech.us/docs/howto/mcp-setup/)
- [OokCite app](https://ookcite.turtletech.us/app)
- [TurtleTech](https://turtletech.us)

## License

MIT. see [LICENSE](LICENSE).
