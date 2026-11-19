# Local metadata overrides

The OokCite API accepts `update_entry_metadata` edits but does not yet store every field this client sends.
The open items are listed in [`ookcite-api-export.md`](ookcite-api-export.md).
Until the API stores them, `ookcite-mcp` keeps each edit in a local file and applies it on export, so the `.bib` still comes only from `export_collection`.

## Which servers keep it

The file is keyed by collection name only, so it belongs to the one person who runs the process.
The process decides once at startup whether it keeps the store:

- The stdio server keeps it.
- `ookcite-mcp serve` with `auth=none` keeps it, since that mode is a copy for one person.
- `ookcite-mcp serve` with bearer or OAuth auth never reads or writes it, since one user's edits would otherwise reach another user's export of a collection with the same name.

On a server without the store, `update_entry_metadata` updates the server record only, `local_only=true` returns an error and changes nothing, and `export_collection` exports the server record alone.

A BibTeX import on a server that keeps the store writes one `source` record per entry into this file. Export prints that record. A shared bearer or OAuth server does not write it, and the import reply says so: export then uses the server record only, so a citation key, a brace group, a name particle, a LaTeX accent, and any field the server does not store can be lost. The fields the server has to persist are listed in [`ookcite-api-export.md`](ookcite-api-export.md).

## Where the file lives

One JSON file per collection:

1. `$OOKCITE_OVERRIDES_DIR/<collection>.json` when `OOKCITE_OVERRIDES_DIR` is set.
2. Otherwise `$XDG_DATA_HOME/ookcite/overrides/<collection>.json`.
3. Otherwise `~/.local/share/ookcite/overrides/<collection>.json`.

Characters outside `A-Z a-z 0-9 . _ -` in the collection name become `_` in the file name.
Each edit reads, changes and writes the file while it holds an exclusive advisory lock on `<collection>.json.lock`, so concurrent edits from several threads or processes are applied one after another.
The file is written to a uniquely named temporary file in the same directory and renamed over the old one, so an interrupted write never leaves half a file and two writers never share a temporary name.

## How edits reach it

`update_entry_metadata` folds every call into the file before it sends the PUT to the server.
The reply names the DOI or entry and the collection, never a path on the machine.
If the server rejects the PUT (for example `entry_type=techreport`, which the API enum refuses), the local override is still saved and the reply says so.
`local_only=true` writes only the local file.

An entry is keyed by `doi:<lowercase DOI>` when it has a DOI, and by `id:<entry id>` otherwise.
A later edit of the same entry updates its record; fields it does not mention are kept.

## Format

```json
{
  "version": 1,
  "collection": "readcon-cpc-v3",
  "entries": {
    "doi:10.1002/jcc.21224": {
      "fields": {
        "citation_key": "martinezPackmolPackageBuilding2009",
        "title": "{PACKMOL}: A package for building initial configurations for molecular dynamics simulations",
        "url": null
      }
    },
    "doi:10.17487/rfc8259": {
      "fields": { "url": null },
      "editor": []
    },
    "doi:10.7717/peerj-cs.86": {
      "authors": [
        { "family": "Smith", "given": "Arfon M." },
        { "family": "FORCE11 Software Citation Working Group", "literal": true }
      ]
    }
  }
}
```

`fields` holds `title`, `journal`, `volume`, `issue`, `pages`, `publisher`, `doi`, `url`, `version`, `eid`, `booktitle`, `entry_type`, `institution`, `chapter`, `number`, `citation_key`, `howpublished` and `note`.
A string replaces the provider value.
`null` removes the field from the export, even when Crossref or DataCite has it.
`authors` and `editor` are person lists; an empty list removes them.
A person with `"literal": true` is one organisation name and exports braced.
`year` replaces the year, and `"year": null` removes it from the export.

## Clearing a field

`clear` in `update_entry_metadata` records a removal marker for each name: `null` in `fields`, an empty `authors` or `editor` list, or `"year": null`.
Export leaves a cleared field out even when Crossref or DataCite supplies a value for it, so `clear=["year"]` drops the provider year too.
Setting the field again later replaces the marker.

## Export

`export_collection` loads the file and, for an entry with a `source` record, prints that entry. A field set later with `update_entry_metadata` replaces the same field in the printed entry. `enrich=true` fills only a field the source left empty, from the resolver. The default is off, so a resolver title, URL, page range, or author list does not replace or extend the source.
`local_overrides=false` exports the server record alone.

An imported entry is printed with these normalisations and no others: the entry type and field names are lower case; fields stay in source order and are written as `name = {value}`; a numeric or quoted value is written in braces; `#` concatenation is joined and an `@string` macro is not expanded; a Unicode en dash or em dash is written `--` or `---`; a bare `&`, `%`, `#`, or `_` is escaped and an escaped one is not escaped again. Braces, `--`, name particles, and LaTeX accents are copied as written. A source file that already has this shape is unchanged.

A file that cannot be read or parsed does not stop the export.
The full path and the error go to stderr, and the BibTeX starts with a `%` comment that names the collection and says the overrides were not applied.

`eid_as` chooses where an article number goes:

- `eid` (default) writes `eid`.
- `pages` writes the article number as `pages` when there is no page range. Use it with elsarticle-num and other styles that never print `eid`.
- `note` writes `Article <number>` in `note` when there is no page range.

## Text cleanup on export

A source entry follows the normalisation list above. For a record that has no source entry, the renderer applies these:

- HTML entities are decoded (`&amp;` becomes `\&`).
- JATS, HTML and MathML tags are removed. A name set in `<tt>`, `<monospace>`, `<code>` or `<scp>` is braced, and a small-caps run after a capital is joined to it (`P<scp>ACKMOL</scp>` becomes `{PACKMOL}`).
- Whitespace is collapsed, with no space before closing punctuation.
- An em dash becomes `---`, an en dash or a spaced hyphen becomes `--`, and Unicode hyphens become `-`.
- Initials print as `S. R.`, not `S.r.`.
- Titles keep acronyms, internal capitals and names braced. In a title-case title the other capitals are left to the style, so a sentence-casing style prints every reference in one case. A lowercase `python`, `gaussian`, `bayesian` or `hessian` exports as the braced name. Brace groups in a caller title are copied unchanged.
- With `software_entry=misc`, a software record gets `howpublished` from its publisher (for example `Zenodo`) and a `note` of `Software, version 0.16.2` or, for a Zenodo concept DOI, `Software, all versions`. A caller `howpublished` or `note` wins.
