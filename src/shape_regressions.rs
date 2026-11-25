//! Regression tests for shapes the OokCite adoption audit saw in the
//! wild. Each case is a synthetic 10.5555 fixture that copies the shape
//! of a real miss but with no live data. The point is to pin the known
//! behaviour so a later refactor does not drift back into the trap.
//!
//! Where the audit found an actual bug the test asserts the fix; where
//! the audit found a near-miss that the current code already dodges the
//! test is a watch, so the dodge is not accidentally removed.

#![cfg(test)]

use serde_json::json;

use crate::bib_export::{merge_provider_metadata, render_pairs, render_single_ris, BibOptions};
use crate::bibliographic::{claim_verdict, ClaimStatus};
use crate::tool_args::BibliographicClaim;
use crate::work_record::detect_consistency_warnings;

#[test]
fn a_record_mixing_title_doi_of_one_work_with_authors_of_another_is_flagged() {
    // Shape (a): the corpus holds a record whose title and DOI belong
    // to work W1 but whose author list came from work W2. The DOI is
    // registered to W1, so a provider fetch for that DOI returns W1's
    // actual authors. The consistency guard must notice the author
    // families do not overlap and flag the record so a caller does not
    // export it as a clean W1 entry.
    let stored = json!({
        "doi": "10.5555/mixed.fields.1",
        "title": "The real W1 title",
        "authors": [
            {"family": "AuthorFromW2", "given": "X."},
            {"family": "AlsoFromW2", "given": "Y."}
        ],
        "volume": "99",
        "pages": "999-1010"
    });
    let provider = json!({
        "crossref": {
            "DOI": "10.5555/mixed.fields.1",
            "title": ["The real W1 title"],
            "author": [
                {"family": "ActualW1Author", "given": "Q."}
            ],
            "container-title": ["Journal of Synthetic Fixtures"],
            "volume": "1",
            "page": "1-10"
        }
    });
    let warnings = detect_consistency_warnings(Some(&stored), Some(&provider));
    let codes: Vec<String> = warnings
        .as_array()
        .expect("warnings is an array")
        .iter()
        .filter_map(|w| w.get("code").and_then(|c| c.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        codes.contains(&"corpus_authors_disagree_with_provider".to_string()),
        "author mix was not flagged: {warnings:#?}"
    );
}

#[test]
fn a_zenodo_concept_doi_record_names_its_version_doi_in_the_bib_entry() {
    // Shape (b): a stored record whose `doi` is the Zenodo *concept*
    // DOI must export a BibTeX entry that also names the *version*
    // DOI (in `url` or a note) so readers can tell which release the
    // entry pointed to. The concept DOI alone is ambiguous: it
    // resolves to whatever release is latest the day the reader
    // follows it.
    //
    // Fixtures use synthetic 10.5555 prefixes rather than a live-
    // looking 10.5281/zenodo.* so the test never shadows a real
    // Zenodo record.
    let concept_doi = "10.5555/concept.zenodo.1";
    let version_doi = "10.5555/concept.zenodo.1.v1";
    let stored = json!({
        "doi": concept_doi,
        "title": "A dataset with a concept DOI",
        "entry_type": "software",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 2026},
        "version": "v1.2.3",
        // The note field is where a BibTeX reader looks for a
        // human-readable disambiguator; the exporter preserves it on
        // `@software` entries (the `url` field is dropped when the
        // url mirrors the DOI prefix, which a Zenodo version URL
        // does). The record ships the version DOI explicitly.
        "note": format!("Zenodo version DOI {version_doi}"),
        "user_fields": ["note"]
    });
    let bib = render_pairs(&[(stored, None)], &BibOptions::default()).expect("entry");
    // The version string survives the export: a reader can see which
    // release the entry pointed at.
    assert!(
        bib.contains("v1.2.3"),
        "the version field was dropped from the entry: {bib}"
    );
    // The *version* DOI reaches the entry (in the note field).
    // The concept DOI would also appear in the `doi` field, which is
    // fine, but the entry must not lose the version identifier.
    assert!(
        bib.contains(version_doi),
        "the version DOI did not reach the entry: {bib}"
    );
}

#[test]
fn a_single_author_record_is_not_truncated_in_the_bib_entry() {
    // Shape (c): a record with exactly one author used to turn into
    // `author = {}` in some export paths because the "truncate at N"
    // path assumed at least two names before `and others`.
    let stored = json!({
        "doi": "10.5555/single.author.1",
        "title": "A paper with one author",
        "entry_type": "journal-article",
        "authors": [{"family": "OnlyAuthor", "given": "A. B."}],
        "date": {"year": 2026}
    });
    let bib = render_pairs(&[(stored, None)], &BibOptions::default()).expect("entry");
    assert!(
        bib.contains("author = {OnlyAuthor, A. B.}"),
        "single author was truncated: {bib}"
    );
    assert!(!bib.contains("author = {}"), "{bib}");
    assert!(!bib.contains("and others"), "{bib}");
}

#[test]
fn claim_verdict_catches_a_year_off_by_one_between_proceedings_and_doi() {
    // Shape (d): the proceedings volume was published in 2024 but the
    // DOI registration date is 2025, so a caller citing "2024" must
    // see MISMATCH, not VALID, even though the DOI exists.
    let meta = json!({
        "doi": "10.5555/proceedings.year.1",
        "title": "A proceedings paper",
        "entry_type": "proceedings-article",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 2025}
    });
    let claim = BibliographicClaim {
        year: Some(2024),
        ..Default::default()
    };
    let verdict = claim_verdict(&meta, &claim);
    assert_eq!(verdict.status, ClaimStatus::Mismatch);
    assert!(
        verdict
            .lines
            .iter()
            .any(|line| line.contains("year: claimed 2024")),
        "{verdict:?}"
    );
}

#[test]
fn claim_verdict_catches_a_chapter_matched_instead_of_the_book() {
    // Shape (e): a reverse_lookup hit on a chapter title when the
    // caller claimed the book title. The chapter record has the book
    // as container (booktitle), so claim_verdict with the book title
    // should refuse the VALID head and flag the title disagreement.
    let meta = json!({
        "doi": "10.5555/chapter.inside.book.1",
        "title": "Chapter 7: A detailed worked example of nucleation dynamics",
        "entry_type": "book-chapter",
        "booktitle": "A Comprehensive Handbook of Synthetic Fixtures and Benchmarks",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 2026}
    });
    let claim = BibliographicClaim {
        title: Some("A Comprehensive Handbook of Synthetic Fixtures and Benchmarks".into()),
        ..Default::default()
    };
    let verdict = claim_verdict(&meta, &claim);
    assert_eq!(
        verdict.status,
        ClaimStatus::Mismatch,
        "chapter matched instead of book without a mismatch: {verdict:?}"
    );
}

#[test]
fn a_manual_entry_type_does_not_turn_into_an_article() {
    // Shape (f): a @manual entry with a version field used to be
    // coerced to @article by the export path, which also dropped the
    // version. This test exports a manual record and asserts the
    // entry type survives and the version is written out.
    let stored = json!({
        "doi": "10.5555/manual.version.1",
        "title": "A manual with a version",
        "entry_type": "manual",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 2026},
        "version": "2.1.0",
        "publisher": "Example Project"
    });
    let bib = render_pairs(&[(stored, None)], &BibOptions::default()).expect("entry");
    assert!(
        bib.contains("@manual{"),
        "manual was coerced away from its type: {bib}"
    );
    assert!(!bib.contains("@article{"), "{bib}");
    assert!(
        bib.contains("2.1.0"),
        "the version was dropped from the export: {bib}"
    );
}

#[test]
fn an_underscore_inside_a_url_field_is_not_backslash_escaped() {
    // Shape (g): a URL with underscores used to come out as
    // `url = {https://host/path\_with\_underscores}` which some BibTeX
    // readers follow verbatim and point at a broken link. The url
    // field should round-trip unchanged.
    let url = "https://example.org/path_with_underscores/_section_1";
    let stored = json!({
        "doi": "10.5555/underscore.url.1",
        "title": "A url with underscores",
        "entry_type": "article",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 2026},
        "url": url
    });
    let bib = render_pairs(&[(stored, None)], &BibOptions::default()).expect("entry");
    assert!(
        bib.contains(&format!("url = {{{url}}}")),
        "url underscores were escaped: {bib}"
    );
    assert!(!bib.contains("\\_"), "{bib}");
}

#[test]
fn an_em_dash_in_the_ris_output_is_preserved_rather_than_turned_into_hyphens() {
    // Shape (h): a title like "Quartz — An efficient synthetic format"
    // was being written to a RIS export as "Quartz --- An efficient
    // synthetic format", because the dash-normalization that TeX needs
    // was running on the RIS path too. RIS consumers parse the triple
    // hyphen as three literal hyphens, which looks wrong to the user.
    let meta = json!({
        "doi": "10.5555/em.dash.ris.1",
        "title": "Quartz\u{2014}An efficient synthetic format",
        "entry_type": "journal-article",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 2026}
    });
    let out = render_single_ris("10.5555/em.dash.ris.1", &meta);
    assert!(
        out.contains("Quartz\u{2014}An efficient"),
        "em dash lost in RIS export: {out}"
    );
    assert!(
        !out.contains("Quartz---An"),
        "RIS turned the em dash into triple hyphens: {out}"
    );
}

#[test]
fn an_arxiv_old_style_id_in_the_eprint_field_is_not_backslash_escaped() {
    // Shape (i): old-style arXiv ids like "cond-mat/0211034" have a
    // slash the BibTeX exporter used to backslash-escape (`\/`),
    // which some readers then rendered as a literal backslash.
    let stored = json!({
        "doi": "10.5555/arxiv.oldstyle.1",
        "title": "An old-style arXiv id",
        "entry_type": "article",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 2002},
        "eprint": "cond-mat/0211034",
        "archivePrefix": "arXiv",
        "user_fields": ["eprint", "archiveprefix"]
    });
    let bib = render_pairs(&[(stored, None)], &BibOptions::default()).expect("entry");
    assert!(
        bib.contains("cond-mat/0211034"),
        "arXiv id was mangled: {bib}"
    );
    assert!(!bib.contains("\\/"), "{bib}");
}

#[test]
fn a_journal_name_with_non_ascii_glyphs_survives_the_merge() {
    // Shape (j): a journal called "Zeitschrift für Physik" used to
    // lose the "ü" through a latin-1 round trip inside the merge
    // path, leaving "Zeitschrift fr Physik" or "Zeitschrift fur
    // Physik" in the export. The merge should pass the glyph through
    // or write the correct TeX accent, never drop it.
    let stored = json!({
        "doi": "10.5555/nonascii.journal.1",
        "title": "A paper in a journal with a diacritic",
        "entry_type": "journal-article",
        "authors": [{"family": "Example", "given": "A."}],
        "date": {"year": 1935},
        "journal": "Zeitschrift f\u{00FC}r Physik"
    });
    let provider = json!({
        "crossref": {
            "DOI": "10.5555/nonascii.journal.1",
            "title": ["A paper in a journal with a diacritic"],
            "container-title": ["Zeitschrift f\u{00FC}r Physik"]
        }
    });
    let merged = merge_provider_metadata(&stored, &provider);
    let journal = merged
        .get("journal")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    assert!(
        journal.contains("f\u{00FC}r")
            || journal.contains("f\\\"ur")
            || journal.contains("f\\\"{u}r"),
        "diacritic lost from journal: {journal:?}"
    );
    assert!(
        !journal.contains("fr Physik"),
        "diacritic dropped entirely: {journal:?}"
    );
    assert!(
        !journal.eq_ignore_ascii_case("zeitschrift fur physik"),
        "diacritic silently normalised to ascii: {journal:?}"
    );
}
