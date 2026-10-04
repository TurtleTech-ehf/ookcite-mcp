---
name: verify-references
description: Check that references exist and match the cited title, authors, year, and pages before submission. Use when the user is checking a bibliography, asks whether a citation is real, or is about to submit a manuscript.
---

Verify with the OokCite connector before the user relies on a citation. A DOI that fails, and a claim that disagrees with the resolved record, stays out of the manuscript.

1. Take DOIs from the references. When a reference has no DOI, call `reverse_lookup` or `batch_resolve` and use a DOI only if the tool returns one. Do not invent a DOI.
2. Call `verify_references` with `dois`. When the reference states a title, authors, year, journal, volume, issue, or pages, pass the same positions in `claims`. Each claim may set `title`, `authors` (family names), `year`, `journal`, `volume`, `issue`, and `pages`. An empty claim only checks that the DOI exists.
3. Treat `MISMATCH` as a failed check, distinct from a DOI that does not exist. Report each item's status in the reply.
4. Prefer `verify_references` over one `validate_doi` call per reference. More than 8 metered items on an anonymous session is refused; ask the user to sign in or split the list.
5. Do not add a failed or mismatched reference to the text the user will submit.
