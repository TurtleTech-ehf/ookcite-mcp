---
name: build-collection
description: Save references into an OokCite collection and export BibTeX or a CSL bibliography. Use when the user wants a library, a saved collection, or an export of references they already resolved.
---

Build the collection with the OokCite connector, then export it. Collection tools need a signed-in account on the hosted connector, or `OOKCITE_API_KEY` on a server the user runs. If a tool reports that sign-in or a key is required, stop and say so.

1. Call `check_duplicates` with `collection` and `query` before adding a work the user may already have saved.
2. For a pasted bibliography, call `import_bibliography` with `content` and `collection`. For a list of DOIs or free-text queries, call `batch_add_to_collection` with `collection` and `queries`. Leave `use_live_queries` false unless the user asks to search beyond the local corpus.
3. Call `export_collection`. Use `format` `bib` for BibTeX. Use `format` `csl`, or a style id such as `ieee`, for formatted bibliography text, and set `style` when `format` is `csl` (default `apa`).
4. Do not call `delete_collection`, `remove_from_collection`, or `unshare_collection` unless the user asked to delete or revoke.
5. Reply with the export text and the collection name. A free account has 4 collections and 200 entries each. `merge_collections` and `batch_move_entries` need an Academic or Business plan.
