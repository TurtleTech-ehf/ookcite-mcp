# Audit a manuscript bibliography

`audit_bibliography` reads a BibTeX file and reports which entries are broken or retracted. It returns citation metadata only. It does not fetch a PDF or the full text.

## Input

One argument:

| Field | Type | Required | Meaning |
| ----- | ---- | -------- | ------- |
| `content` | string | yes | The manuscript bibliography in BibTeX |

```json
{
  "content": "@article{key,\n  title = {Title},\n  author = {Family, Given},\n  year = {2020},\n  doi = {10.1000/example}\n}\n"
}
```

A DOI is taken from the `doi` field. When that field is absent, a `url` that points at `doi.org` or `dx.doi.org` is used. An entry with neither is `NO DOI`. The tool does not guess a DOI for it. A `doi` field that is not a DOI is `BROKEN`. The summary's "with a DOI" count is the number of entries whose field parsed as a DOI, so that broken field is not included.

`@string`, `@preamble`, and `@comment` are skipped. A value may use braces, quotes, or `#` concatenation.

RIS is not accepted. Import RIS with `import_bibliography`, or export BibTeX first.

## What each entry is compared with

For every DOI the tool checks three things, in order:

1. The DOI is registered at doi.org (`responseCode` 1). Anything else is `BROKEN`.
2. The Crossref or DataCite record. A retraction, withdrawal, or expression of concern is read from that record.
3. The title, authors, year, journal, volume, issue, and pages written in the `.bib`, against the resolved record. A disagreement is `MISMATCH`. A supplied field the record does not carry is `INCOMPLETE`.

Author names are family names. `Smith, Jane` and `Jane Smith` are the same claim. `number` is the issue for `@article`, `@inproceedings`, `@incollection`, and `@inbook`.

A collection membership does not skip the lookup. Retraction is not stored on the collection entry.

## Publisher notices

Crossref:

- `update-to[].type` of `retraction`, `withdrawal`, `expression_of_concern`, `correction`, or `erratum`
- `relation` keys `is-retracted-by`, `is-withdrawn-by`, `is-expression-of-concern`, and `is-corrected-by`
- a title that begins with `RETRACTED:` or `WITHDRAWN:`

DataCite:

- `relatedIdentifiers[].relationType` of `IsRetractedBy` or `IsWithdrawnBy`

`Retracts` on a DataCite record means this entry is the notice, not the retracted work, so it is not marked retracted. A correction or erratum is reported on the line and does not change an otherwise `OK` entry.

The date is taken from Crossref `updated.date-parts`. A missing month or day is left out. The tool does not invent one.

## Statuses

Each entry gets one status. The summary counts those statuses. The counts add up to the number of BibTeX entries that were parsed.

| Status | Meaning |
| ------ | ------- |
| `OK` | Registered, publisher record read, no retraction, withdrawal, or expression of concern, and every supplied field agrees |
| `RETRACTED` | The publisher record says this work was retracted |
| `WITHDRAWN` | The publisher record says this work was withdrawn |
| `CONCERN` | The publisher record carries an expression of concern |
| `BROKEN` | The DOI is not registered, or the `doi` field is not a DOI |
| `MISMATCH` | A supplied field disagrees with the resolved record |
| `INCOMPLETE` | A supplied field is absent from the record, and nothing disagreed |
| `UNCHECKED` | The work resolved, but no publisher record was read, so it is not called clean |
| `NO DOI` | The entry has no DOI |
| `UPSTREAM` | The lookup was rate-limited, timed out, or the API returned an error. This is not a verdict on the citation |

When a retraction and a field disagreement are both present, the status is `RETRACTED` and the field lines are still printed.

## Quota

Each distinct DOI is one metered lookup. Repeated DOIs in the same file are looked up once. An anonymous session refuses a file with more than the anonymous batch cap before any lookup. A signed-in session refuses when the remaining daily lookups cannot cover the file. `usage` reports what is left.
