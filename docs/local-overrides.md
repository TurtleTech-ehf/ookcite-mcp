# Local metadata overrides

The OokCite API accepts `update_entry_metadata` edits but does not yet store every field this client sends.
The open items are listed in [`ookcite-api-export.md`](ookcite-api-export.md).
Until the API stores them, `ookcite-mcp` keeps each edit in a local file and applies it on export, so the `.bib` still comes only from `export_collection`.

## Where the file lives

One JSON file per collection:

1. `$OOKCITE_OVERRIDES_DIR/<collection>.json` when `OOKCITE_OVERRIDES_DIR` is set.
2. Otherwise `$XDG_DATA_HOME/ookcite/overrides/<collection>.json`.
3. Otherwise `~/.local/share/ookcite/overrides/<collection>.json`.

Characters outside `A-Z a-z 0-9 . _ -` in the collection name become `_` in the file name.
The file is written through a temporary sibling and renamed, so an interrupted write never leaves half a file.

## How edits reach it

`update_entry_metadata` folds every call into the file before it sends the PUT to the server.
The reply names the file.
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
`year` replaces the year.

## Export

`export_collection` loads the file, lays each override over the server record, and marks every overridden field as caller-set, so the provider value does not replace it.
`local_overrides=false` exports the server record alone.

`eid_as` chooses where an article number goes:

- `eid` (default) writes `eid`.
- `pages` writes the article number as `pages` when there is no page range. Use it with elsarticle-num and other styles that never print `eid`.
- `note` writes `Article <number>` in `note` when there is no page range.

## Text cleanup on export

The renderer applies these to every export, with or without overrides:

- HTML entities are decoded (`&amp;` becomes `\&`).
- JATS, HTML and MathML tags are removed. A name set in `<tt>`, `<monospace>`, `<code>` or `<scp>` is braced, and a small-caps run after a capital is joined to it (`P<scp>ACKMOL</scp>` becomes `{PACKMOL}`).
- Whitespace is collapsed, with no space before closing punctuation.
- An em dash becomes `---`, an en dash or a spaced hyphen becomes `--`, and Unicode hyphens become `-`.
- Initials print as `S. R.`, not `S.r.`.
- Titles keep acronyms, internal capitals and names braced. In a title-case title the other capitals are left to the style, so a sentence-casing style prints every reference in one case. A lowercase `python`, `gaussian`, `bayesian` or `hessian` exports as the braced name. Brace groups in a caller title are copied unchanged.
- With `software_entry=misc`, a software record gets `howpublished` from its publisher (for example `Zenodo`) and a `note` of `Software, version 0.16.2` or, for a Zenodo concept DOI, `Software, all versions`. A caller `howpublished` or `note` wins.
