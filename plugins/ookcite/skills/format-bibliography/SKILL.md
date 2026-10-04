---
name: format-bibliography
description: Format a pasted bibliography into BibTeX or a CSL style. Use when the user pastes a reference list, asks for BibTeX, or wants citations rendered in a named style such as APA or IEEE.
---

Format the bibliography with the OokCite connector. Return citation metadata only. Do not fetch PDFs or full text.

1. For one DOI, call `format_citation` with `doi` and `style`. The default style is `apa`.
2. For several citations that should come back in one style, call `batch_format` with `citations` and `style`. Leave `use_live_queries` false unless the user asks to search beyond the local corpus.
3. For a pasted numbered list, blank-line list, BibTeX, or RIS, call `import_bibliography`. Set `format` to `plaintext`, `bibtex`, `ris`, or `auto`. Omit `collection` when the user wants the text back and does not want it saved. Pass `style` when they also want CSL text.
4. A paste with no collection is limited to 8 metered items and the anonymous daily cap. A longer list needs a signed-in account and the build-collection skill.
5. Reply with the bibliography text. Do not paste the raw tool payload.
