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
