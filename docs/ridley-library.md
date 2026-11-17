# Ridley library tools

The Ridley MCP tools have two modes. Local mode is the default read path:

```json
{
  "env": {
    "OOKCITE_RIDLEY_MODE": "local",
    "OOKCITE_RIDLEY_REPLICA": "/home/me/.local/share/ttech/library.redb"
  }
}
```

`ookcite-mcp` invokes `tt library` with `TT_RIDLEY_REPLICA` set to that
path. `tt` opens the replica through ridley-core. Set `OOKCITE_RIDLEY_TT`
only when the `tt` executable is not on `PATH`.

Local mode supports:

- `search_library`: title, author, tag, note, annotation/highlight and PDF
  full-text search.
- `get_item`: metadata, notes and highlights.
- `export_bib`: collection export from the synced replica.
- `list_libraries`: libraries represented in the local replica.

`add_by_doi` always uses the signed-in dashboard credential and posts the DOI
to `POST /api/v1/ridley/libraries/{id}/inbox`. The inbox is consumed by a
Ridley client; the dashboard does not write a second, incompatible library.

## Why reads are local

The dashboard cannot decrypt synced Ridley objects:

- `ridley-core/src/v2/object_seal.rs:270-294` opens an object only after its
  16-byte key id matches and its authenticated decryption succeeds.
- `ttech-dashboard/src/api/ridley_keys.rs:105-109` returns `key_b64` and a
  wire version but no object key id.
- `ttech-dashboard/migrations/025_ridley_library_keys.sql:13-19` stores the
  encrypted data key and wire version, but no object key id.

The existing dashboard library endpoints are a share catalogue, not the
synced replica:

- `ttech-dashboard/src/services/library_bulk.rs:91-107` exposes item id,
  title, tags and collection membership.
- `ttech-dashboard/src/services/library_views.rs:88-109` exposes table
  metadata such as title, author, DOI and tags.
- `ttech-dashboard/src/services/library_views.rs:558-560` loads that metadata
  from `library_metadata_files`.

Neither endpoint can return sealed notes, highlights or PDF text.

## Explicit metadata-only fallback

Set `OOKCITE_RIDLEY_MODE=remote` only when catalogue metadata is sufficient.
In this mode `search_library`, `get_item`, and `export_bib` use
`GET /libraries/{id}/table`, `GET /libraries/{id}/items`, and
`POST /libraries/{id}/table/export`. Responses are scoped by the bearer from
`setup --connect`. `get_item` labels notes, highlights and full text as
unavailable instead of pretending the catalogue is the synced library.
