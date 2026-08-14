# Changelog
All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

- - -
## v0.5.0 - 2026-08-14
#### Features
- (**mcp**) ship a credential helper that keeps the key out of client config - (7448748) - Rohit Goswami
- (**mcp**) upfront quota and collection membership for batches - (fc912ee) - Rohit Goswami
- (**mcp**) doctor tool, mutate policy, setup-help errors - (9a1fea5) - Rohit Goswami
- (**mcp**) tool annotations, faster startup, multi-client setup docs - (540fc5d) - Rohit Goswami
- (**registry**) publish an MCP registry manifest - (86402fb) - Rohit Goswami
#### Bugfixes
- (**batch**) preserve DOI rate limits - (69c66e7) - Rohit Goswami
- (**ci**) satisfy retry status lint - (6e5a883) - Rohit Goswami
- (**ci**) satisfy Rust 1.97 option lint - (99eaf1c) - Rohit Goswami
- (**ci**) place tests after module items - (b00ecbe) - Rohit Goswami
- (**contract**) align published OokCite capabilities - (58393d2) - Rohit Goswami
- (**debug**) include candidate diagnostics - (e2f32a1) - Rohit Goswami
- (**debug**) render final candidate lists - (476ee90) - Rohit Goswami
- (**debug**) tag text resolve requests - (67ea984) - Rohit Goswami
- (**diagnostics**) align anonymous quota guidance - (137cc3f) - Rohit Goswami
- (**http**) stop retrying rate limits - (330f8d8) - Rohit Goswami
- (**mcp**) membership-only meta for batches; preflight group_cite/keys - (611758a) - Rohit Goswami
- (**quota**) align anonymous client guidance - (2d9cd0b) - Rohit Goswami
- (**resolve**) check the resolver answer against the ranked candidates - (6889a7f) - Rohit Goswami
- (**reverse**) use /api/v1/reverse and show authors - (5eb5628) - Rohit Goswami
#### Documentation
- (**grok**) add plugin surface and Grok Build setup - (5eec877) - Rohit Goswami
- (**setup**) publish portable MCP configuration - (c36fd94) - Rohit Goswami
- source layout, lib crate map, and Unreleased changelog - (8350b1f) - Rohit Goswami
#### Tests
- (**batch**) stop DOI fallback after rate limit - (24b458e) - Rohit Goswami
- (**contract**) assert current plan guidance - (d0a3939) - Rohit Goswami
- (**contract**) use current OokCite plan fixtures - (8cb2be0) - Rohit Goswami
- (**contract**) cover npm and feature gates - (050778c) - Rohit Goswami
- (**debug**) require candidate diagnostics - (5c2a656) - Rohit Goswami
- (**debug**) cover candidate-list responses - (1ec643d) - Rohit Goswami
- (**debug**) require tagged resolve input - (43bd415) - Rohit Goswami
- (**http**) reject retries after rate limits - (65a2a7a) - Rohit Goswami
- (**quota**) cover paid plan limits and prices - (78ae753) - Rohit Goswami
- (**quota**) pin public OokCite limits - (c4d7b45) - Rohit Goswami
#### Refactors
- (**mcp**) move each tool's trigger into its own description - (f1b62bb) - Rohit Goswami
- (**test**) parse OokCite plan rows structurally - (9e5cb9b) - Rohit Goswami
- split main.rs into focused modules - (d50538d) - Rohit Goswami
#### Style
- (**contract**) apply Rust formatting - (ea7f3c5) - Rohit Goswami
- apply rustfmt - (c1352b8) - Rohit Goswami

- - -

## Unreleased
#### Features
- (**mcp**) tool annotations (read-only / destructive / idempotent hints) on shipped tools for protocol-aware clients
- (**startup**) skip blocking auth/npm probes on MCP connect by default (`OOKCITE_STARTUP_PROBES=1` restores them on stderr)
- (**http**) shared API client with connect timeout, pool reuse, and TCP keepalive
#### Refactors
- (**structure**) split binary `main.rs` into `server`, `tool_args`, `cli`, `collection_entries`, `resolve_helpers`, `http_error`, `constants` (behavior unchanged; see README Source layout)
#### Documentation
- (**readme**) source layout, contributing checks, entry_id/DOI remove notes (carried from v0.4.10)
- (**crate**) `lib.rs` documents binary module map and public `endpoints` surface
- (**grok**) Grok Build plugin surface (`.mcp.json`, `plugin.json`) and client setup in README/npm README
- (**clients**) setup prints Grok/Claude/Codex-specific install paths; README covers env knobs and batch/destructive usage tips

- - -
## v0.4.10 - 2026-06-22
#### Bugfixes
- (**collections**) resolve entry IDs by DOI alias for remove/search - (60f8b4f) - Rohit Goswami
#### Documentation
- (**mcp**) document entry_id and DOI alias remove/search workflow - (d0d1d61) - Rohit Goswami

- - -
## v0.4.9 - 2026-06-13
#### Bugfixes
- (**collections**) surface entry deletion results - (cc44ec8) - Rohit Goswami
- (**reverse**) reject unconfident live retries - (202f7dd) - Rohit Goswami
- (**reverse**) retry weak local matches live - (12ffd64) - Rohit Goswami
#### Tests
- (**collections**) cover mcp entry deletion flow - (093a88e) - Rohit Goswami
- (**contract**) refresh encrypted fixture - (e96c67f) - Rohit Goswami
- (**contract**) refresh OpenAPI fixture - (d4d0932) - Rohit Goswami
#### Continuous Integration
- (**release**) pin npm trusted publishing cli - (2e2d61a) - Rohit Goswami
- (**release**) use trusted npm publishing - (796b381) - Rohit Goswami
#### Maintenance
- (**npm**) normalize repository url - (3d7da56) - Rohit Goswami
#### Style
- (**clippy**) simplify collection lookup - (71034f9) - Rohit Goswami
- (**reverse**) match release formatting - (7c31d55) - Rohit Goswami

- - -

## v0.4.6 - 2026-04-23
#### Bugfixes
- (**validate**) share doi lookup semantics with verify_references - (a537de5) - Rohit Goswami
#### Documentation
- (**mcp**) clarify restart and collection workflow - (39c6251) - Rohit Goswami

- - -

Changelog generated by [cocogitto](https://github.com/cocogitto/cocogitto).