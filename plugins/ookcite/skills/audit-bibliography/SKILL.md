---
name: audit-bibliography
description: Check a manuscript .bib for broken DOIs and publisher retractions before submission. Use when the user pastes or attaches BibTeX, asks whether references are retracted, or is about to submit a manuscript that already has a .bib file.
---

Audit the bibliography with the OokCite connector. Return citation metadata only. Do not fetch PDFs or full text.

1. When the user has BibTeX, call `audit_bibliography` with the file contents in `content`. Do not retype the entries into `verify_references` first.
2. Report every status the tool returns. `RETRACTED`, `WITHDRAWN`, `CONCERN`, `BROKEN`, and `MISMATCH` stay out of the manuscript until the user fixes them. `UNCHECKED` is not a clean bill: the publisher record was not read.
3. `NO DOI` means the entry had no DOI. Do not invent one. Offer `reverse_lookup` only if the user asks to search for that entry.
4. A correction line on an `OK` entry is a notice, not a failure.
5. More than 40 DOIs on an anonymous session is refused. Ask the user to sign in or split the file. A saved collection does not skip this audit.
