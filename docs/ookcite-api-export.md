# OokCite API changes for publication BibTeX

`ookcite-mcp` renders collection BibTeX itself from Crossref, DataCite, Zenodo, and doi.org. `/api/v1/collections/{id}/export.bib` is not the publication file. The API still has to change so other clients get the same records. The client sends the fields below on `PUT /api/v1/collections/{id}/entries/{eid}/metadata`. The server must persist them.

1. `export.bib` must set `booktitle` from the Crossref container title. For a proceedings article use the first container title, otherwise the event name. For a book chapter use the container title that is not a series or lecture-notes title, otherwise the last container title. Do not leave the venue only in `journal` or `publisher`.

2. Map provider types on export and on stored records. `journal-article` is `@article`. `proceedings-article` is `@inproceedings`. `book-chapter` is `@incollection` and includes `chapter` and `pages`. `report` is `@techreport` with `institution` and `number`. An RFC uses the number from the DOI and `institution` `IETF`. `preprint` and `posted-content` are `@misc` with `howpublished` `arXiv:<id>`, not `@techreport`. `standard` is `@misc` with a literal organisational author. DataCite `Software` and Zenodo records are `@software`, or `@misc` with `howpublished` `Software` when the caller asks for a style that has no `@software`. Persist and emit a caller-supplied `entry_type`.

3. For an article or proceedings paper, BibTeX `number` is the issue. The article number is `eid`, not a page range of the form `134106--134106`. When `page` and the article number are the same string, emit `pages` once and omit `eid`. For a report, `number` is the report number.

4. Persist and emit a caller-supplied `citation_key`. Do not regenerate it on export.

5. `CitationMetadata` must accept and persist `version`, `eid`, `booktitle`, `entry_type`, `institution`, `chapter`, `pages`, `number`, `citation_key`, and `user_fields`. A replacement `PUT` must treat a missing optional key, or JSON null, as deletion of that key. Keep required `id`, `entry_type`, `title`, and `authors`. Do not answer 400 because one of these fields is present. `user_fields` lists keys the caller set; export must prefer those over a provider value. A key absent from the replacement object is deleted, including `editor`, `affiliation`, and `issn`.

6. MCP `isError` is set by this client. The HTTP API does not need a change for that flag.

7. Reject a DOI whose doi.org handle `responseCode` is not 1. HTTP 404 with `{"responseCode":100}` is unregistered. Do not return that DOI as an `INCOMPLETE` or `VALID` record.

8. Keep a Zenodo concept DOI as the concept DOI. Do not replace it with one version's title or with GitHub login creators. A version DOI is the record whose DataCite relation is `IsVersionOf` the concept. A concept DOI is the record that `HasVersion` and is not itself a version. Label the two cases. Drop creator names that are GitHub logins.

9. `/api/v1/resolve` must score `paper` with that candidate's score. Do not imply confidence 100. Do not return an unregistered DOI. An empty `/api/v1/reverse` result must still be allowed to fall through to live resolve.

10. An organisational author is one literal name. BibTeX is `{{IEEE Computer Society}}`, not `{IEEE Computer Society}, {given}`.

11. Journal abbreviation checks live in this client. The API does not need to compare ISO 4 forms unless it grows its own verifier.

12. Store the full Crossref author list. The Frontiers record `10.3389/fchem.2026.1807063` has three authors (Goswami, Gunde, Jónsson), not one.

13. Protect acronyms and internal capitals in titles with braces. Omit `url` when it is only the DOI URL (`doi.org`, `dx.doi.org`, `doi.org/doi/`, or `zenodo.org/doi/` of the same DOI). Keep a different landing-page URL.

14. Persist an imported bibliography entry as it was supplied, and return those fields on `GET /api/v1/collections/{id}`. A replacement `PUT` must accept every field below and must not answer 400 because one of them is present. A missing key on a replacement deletes that key. Do not add a field the import did not carry (`issn`, `url`, `publisher`, `abstract`, `affiliation`), and do not replace a URL the import did carry.

    - `citation_key`, exactly as imported. Do not generate a new key when one was supplied. Generate one only when the source entry has no key.
    - `entry_type` as supplied: `article`, `book`, `booklet`, `conference`, `inbook`, `incollection`, `inproceedings`, `manual`, `mastersthesis`, `misc`, `phdthesis`, `proceedings`, `techreport`, `unpublished`, `software`, `patent`, `online`, `dataset`. Do not map `manual` or `techreport` to `misc`, `incollection` to `inbook`, or `misc` to `software`.
    - `title`, raw: brace groups, `--`, and LaTeX commands stay as written.
    - `authors` and `editors`, each person as `family`, `given`, `prefix` (von), `suffix` (jr), and `literal`. A name in braces is one literal organisation. Do not move `De` or `Le` after the given name. Do not shorten a given name to initials. Do not replace a literal with resolver person names. Do not turn `{\o}`, `\'{e}`, or `{\v{z}}` into a Unicode letter or drop the accent.
    - `journal`, `booktitle`, `chapter`, `series`, `volume`, `number` (the issue or the report number; keep `--`), `pages` (keep a source page range, including an article number and a single hyphen), `publisher` (keep `\&`), `address`, `edition`, `organization`, `institution`, `type` (the BibTeX `type` field, distinct from `entry_type`), `howpublished`, `note`, `version`, `school`, `month`, `isbn`, `doi`, `url`, `year`.
    - `user_fields`, the keys the caller set.

    A resolver value may fill a field only when that field is empty in the source, the caller did not clear it, and the client asked for enrichment. It must not replace `volume`, `number`, `pages`, or any other non-empty source value, and it must not change the key or the entry type. `doi`, `url`, and `eprint` are copied verbatim. A month or other field that names an `@string` macro stays a bare macro. Two entries that share a DOI are both kept.
