//! Compare a resolved DOI record with the bibliographic claim a caller
//! supplied. A DOI that exists can still be a different paper. Numeric
//! fields disagree exactly. Journal names agree after abbreviation
//! expansion. A supplied field the record does not carry is unchecked,
//! and an unchecked field is not an agreement.

use serde_json::Value;

use crate::tool_args::BibliographicClaim;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Match,
    Mismatch,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Report {
    status: Status,
    lines: Vec<String>,
}

pub fn format_verify_line(doi: &str, meta: &Value, claim: Option<&BibliographicClaim>) -> String {
    let title = meta_str(meta, "title").unwrap_or("?");
    let Some(claim) = claim.filter(|claim| !claim.is_empty()) else {
        return format!("VALID {doi} : {title}");
    };
    let report = compare(meta, claim);
    match report.status {
        Status::Match => format!("VALID {doi} : {title}"),
        Status::Mismatch => join_verdict("MISMATCH", doi, title, &report.lines),
        Status::Incomplete => join_verdict("INCOMPLETE", doi, title, &report.lines),
    }
}

pub fn format_validate_doi(
    requested_doi: &str,
    meta: &Value,
    claim: &BibliographicClaim,
) -> String {
    let title = meta_str(meta, "title").unwrap_or("?");
    let authors = meta["authors"]
        .as_array()
        .map(|authors| {
            authors
                .iter()
                .filter_map(|author| author["family"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let year = meta["date"]["year"]
        .as_i64()
        .map(|year| year.to_string())
        .unwrap_or_default();
    let journal = meta_str(meta, "journal").unwrap_or("N/A");
    let volume = meta_str(meta, "volume").unwrap_or("N/A");
    let issue = meta_str(meta, "issue").unwrap_or("N/A");
    let pages = meta_str(meta, "pages").unwrap_or("N/A");
    let doi = meta_str(meta, "doi").unwrap_or(requested_doi);
    let report = if claim.is_empty() {
        None
    } else {
        Some(compare(meta, claim))
    };
    let head = match report.as_ref().map(|report| report.status) {
        None | Some(Status::Match) => "VALID",
        Some(Status::Mismatch) => "MISMATCH",
        Some(Status::Incomplete) => "INCOMPLETE",
    };
    let mut out = format!(
        "{head}\nDOI: {doi}\nTitle: {title}\nAuthors: {authors}\nYear: {year}\nJournal: {journal}\nVolume: {volume}\nIssue: {issue}\nPages: {pages}"
    );
    if let Some(report) = report {
        if report.lines.is_empty() {
            out.push_str("\nClaim: agrees");
        } else {
            for line in report.lines {
                out.push_str("\nClaim: ");
                out.push_str(&line);
            }
        }
    }
    out
}

fn join_verdict(head: &str, doi: &str, title: &str, lines: &[String]) -> String {
    let mut out = vec![format!("{head} {doi} : {title}")];
    out.extend(lines.iter().cloned());
    out.join("\n")
}

fn compare(meta: &Value, claim: &BibliographicClaim) -> Report {
    let mut lines = Vec::new();
    let mut disagreed = false;
    let mut unchecked = false;

    if let Some(title) = claim.title.as_deref().filter(|s| !s.trim().is_empty()) {
        judge(
            "title",
            title,
            meta_str(meta, "title"),
            title_relation,
            &mut lines,
            &mut disagreed,
            &mut unchecked,
        );
    }
    if let Some(authors) = claim.authors.as_ref().filter(|names| !names.is_empty()) {
        match author_relation(authors, meta) {
            Relation::Agree => {}
            Relation::Disagree(detail) => {
                lines.push(format!("authors: {detail}"));
                disagreed = true;
            }
            Relation::Unchecked(detail) => {
                lines.push(format!("authors: {detail}"));
                unchecked = true;
            }
        }
    }
    if let Some(year) = claim.year {
        let record = meta["date"]["year"]
            .as_i64()
            .or_else(|| meta["year"].as_i64());
        match record {
            Some(record) if record == i64::from(year) => {}
            Some(record) => {
                lines.push(format!("year: claimed {year}, record {record}"));
                disagreed = true;
            }
            None => {
                lines.push(format!("year: claimed {year}, record has none"));
                unchecked = true;
            }
        }
    }
    if let Some(journal) = claim.journal.as_deref().filter(|s| !s.trim().is_empty()) {
        judge(
            "journal",
            journal,
            meta_str(meta, "journal"),
            journal_relation,
            &mut lines,
            &mut disagreed,
            &mut unchecked,
        );
    }
    if let Some(volume) = claim.volume.as_deref().filter(|s| !s.trim().is_empty()) {
        judge(
            "volume",
            volume,
            meta_str(meta, "volume"),
            |claim, record| ident_relation(claim, record, norm_numish),
            &mut lines,
            &mut disagreed,
            &mut unchecked,
        );
    }
    if let Some(issue) = claim.issue.as_deref().filter(|s| !s.trim().is_empty()) {
        judge(
            "issue",
            issue,
            meta_str(meta, "issue"),
            |claim, record| ident_relation(claim, record, norm_numish),
            &mut lines,
            &mut disagreed,
            &mut unchecked,
        );
    }
    if let Some(pages) = claim.pages.as_deref().filter(|s| !s.trim().is_empty()) {
        judge(
            "pages",
            pages,
            meta_str(meta, "pages"),
            pages_relation,
            &mut lines,
            &mut disagreed,
            &mut unchecked,
        );
    }

    let status = if disagreed {
        Status::Mismatch
    } else if unchecked {
        Status::Incomplete
    } else {
        Status::Match
    };
    Report { status, lines }
}

enum Relation {
    Agree,
    Disagree(String),
    Unchecked(String),
}

fn judge(
    field: &str,
    claimed: &str,
    record: Option<&str>,
    relate: impl Fn(&str, &str) -> Relation,
    lines: &mut Vec<String>,
    disagreed: &mut bool,
    unchecked: &mut bool,
) {
    let Some(record) = record.filter(|value| !value.trim().is_empty()) else {
        lines.push(format!("{field}: claimed {claimed}, record has none"));
        *unchecked = true;
        return;
    };
    match relate(claimed, record) {
        Relation::Agree => {}
        Relation::Disagree(detail) => {
            lines.push(format!("{field}: {detail}"));
            *disagreed = true;
        }
        Relation::Unchecked(detail) => {
            lines.push(format!("{field}: {detail}"));
            *unchecked = true;
        }
    }
}

fn ident_relation(claimed: &str, record: &str, norm: impl Fn(&str) -> String) -> Relation {
    if norm(claimed) == norm(record) {
        Relation::Agree
    } else {
        Relation::Disagree(format!("claimed {claimed}, record {record}"))
    }
}

fn meta_str<'a>(meta: &'a Value, key: &str) -> Option<&'a str> {
    meta[key].as_str().filter(|value| !value.is_empty())
}

fn norm_numish(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        let stripped = trimmed.trim_start_matches('0');
        if stripped.is_empty() {
            "0".to_string()
        } else {
            stripped.to_string()
        }
    } else {
        trimmed.to_ascii_lowercase()
    }
}

fn pages_relation(claimed: &str, record: &str) -> Relation {
    let Some(claim) = page_span(claimed) else {
        return ident_relation(claimed, record, |s| s.trim().to_ascii_lowercase());
    };
    let Some(record_span) = page_span(record) else {
        return ident_relation(claimed, record, |s| s.trim().to_ascii_lowercase());
    };
    let agrees = match (claim, record_span) {
        ((start, None), (record_start, Some(end))) => start >= record_start && start <= end,
        ((start, None), (record_start, None)) => start == record_start,
        ((start, Some(end)), (record_start, Some(record_end))) => {
            start == record_start && end == record_end
        }
        ((start, Some(_)), (record_start, None)) => start == record_start,
    };
    if agrees {
        Relation::Agree
    } else {
        Relation::Disagree(format!("claimed {claimed}, record {record}"))
    }
}

fn page_span(value: &str) -> Option<(u32, Option<u32>)> {
    let mut nums = Vec::new();
    let mut current = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            nums.push(current.parse::<u32>().ok()?);
            current.clear();
        }
    }
    if !current.is_empty() {
        nums.push(current.parse::<u32>().ok()?);
    }
    match nums.as_slice() {
        [start] => Some((*start, None)),
        [start, end, ..] => Some((*start, Some(*end))),
        [] => None,
    }
}

fn author_relation(claimed: &[String], meta: &Value) -> Relation {
    let record = record_families(meta);
    if record.is_empty() {
        return Relation::Unchecked(format!("claimed {}, record has none", claimed.join(", ")));
    }
    let absent: Vec<&str> = claimed
        .iter()
        .filter(|name| {
            let family = family_of(name);
            !record.iter().any(|have| have.norm == family)
        })
        .map(String::as_str)
        .collect();
    if absent.is_empty() {
        Relation::Agree
    } else {
        let shown = record
            .iter()
            .map(|family| family.display.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Relation::Disagree(format!(
            "claimed {} absent from record {shown}",
            absent.join(", ")
        ))
    }
}

struct Family {
    norm: String,
    display: String,
}

fn record_families(meta: &Value) -> Vec<Family> {
    meta["authors"]
        .as_array()
        .map(|authors| {
            authors
                .iter()
                .filter_map(|author| {
                    let display = author["family"]
                        .as_str()
                        .map(str::trim)
                        .filter(|family| !family.is_empty())
                        .map(str::to_string)
                        .or_else(|| author.as_str().map(|name| name.to_string()))?;
                    let norm = family_of(&display);
                    if norm.is_empty() {
                        None
                    } else {
                        Some(Family { norm, display })
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn family_of(name: &str) -> String {
    let trimmed = name.trim();
    if let Some((family, _)) = trimmed.split_once(',') {
        return norm_token(family);
    }
    trimmed
        .split_whitespace()
        .next_back()
        .map(norm_token)
        .unwrap_or_default()
}

fn norm_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn journal_relation(claimed: &str, record: &str) -> Relation {
    let claim_key = journal_key(claimed);
    let record_key = journal_key(record);
    if claim_key == record_key {
        return Relation::Agree;
    }
    let claim_tokens: Vec<&str> = claim_key.split_whitespace().collect();
    if claim_tokens.len() == 1 {
        let token = claim_tokens[0];
        let known = record_key.split_whitespace().any(|word| word == token);
        if !known && token.len() <= 6 && token.chars().all(|ch| ch.is_ascii_alphabetic()) {
            return Relation::Unchecked(format!(
                "claimed {claimed}, record {record} (abbreviation not compared)"
            ));
        }
    }
    Relation::Disagree(format!("claimed {claimed}, record {record}"))
}

fn journal_key(value: &str) -> String {
    let joined = journal_tokens(value).join(" ");
    match joined.as_str() {
        "pnas" => "proceedings national academy sciences",
        "jacs" => "journal american chem society",
        "jctc" => "journal chem theory computation",
        "jcp" => "journal chem phys",
        "jpc" => "journal phys chem",
        "jpca" => "journal phys chem a",
        "jpcb" => "journal phys chem b",
        "jpcl" => "journal phys chem letters",
        "prl" => "phys review letters",
        "prb" => "phys review b",
        "pra" => "phys review a",
        "pre" => "phys review e",
        other => other,
    }
    .to_string()
}

fn journal_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for raw in value.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_ascii_lowercase();
        let expanded = match lower.as_str() {
            "the" | "of" | "and" | "a" | "an" | "for" | "in" => continue,
            "j" | "jour" | "jrnl" | "journal" => "journal",
            "phys" | "physical" | "physics" => "phys",
            "chem" | "chemical" | "chemistry" => "chem",
            "rev" | "review" | "reviews" => "review",
            "lett" | "letter" | "letters" => "letters",
            "sci" => "science",
            "proc" => "proceedings",
            "natl" => "national",
            "am" | "amer" => "american",
            "int" | "intl" => "international",
            "comm" | "commun" => "communications",
            "res" => "research",
            "soc" => "society",
            "acad" => "academy",
            "biol" => "biological",
            "med" => "medical",
            "eng" => "engineering",
            "appl" => "applied",
            "theor" => "theoretical",
            "comp" | "comput" => "computation",
            "trans" => "transactions",
            _ => &lower,
        };
        tokens.push(expanded.to_string());
    }
    tokens
}

fn title_relation(claimed: &str, record: &str) -> Relation {
    let claim = content_tokens(claimed);
    let record_tokens = content_tokens(record);
    if claim.is_empty() || record_tokens.is_empty() {
        return Relation::Unchecked(format!("claimed {claimed}, record {record}"));
    }
    if is_subsequence(&claim, &record_tokens) || is_subsequence(&record_tokens, &claim) {
        return Relation::Agree;
    }
    let shared = claim
        .iter()
        .filter(|token| record_tokens.iter().any(|have| have == *token))
        .count();
    let union = claim.len() + record_tokens.len() - shared;
    let jaccard = if union == 0 {
        1.0
    } else {
        shared as f64 / union as f64
    };
    if claim.len() >= 4 && record_tokens.len() >= 4 && jaccard < 0.25 {
        Relation::Disagree(format!("claimed {claimed}, record {record}"))
    } else {
        Relation::Agree
    }
}

fn content_tokens(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() > 2)
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| {
            !matches!(
                token.as_str(),
                "the" | "and" | "for" | "with" | "from" | "its"
            )
        })
        .collect()
}

fn is_subsequence(needle: &[String], hay: &[String]) -> bool {
    let mut rest = hay;
    for token in needle {
        match rest.iter().position(|have| have == token) {
            Some(index) => rest = &rest[index + 1..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn voth_1993() -> Value {
        json!({
            "doi": "10.1021/j100134a002",
            "title": "Feynman path integral formulation of quantum mechanical transition-state theory",
            "authors": [{"family": "Voth", "given": "Gregory A."}],
            "date": {"year": 1993},
            "journal": "The Journal of Physical Chemistry",
            "volume": "97",
            "issue": "32",
            "pages": "8365-8377"
        })
    }

    fn claim_1989_jpc() -> BibliographicClaim {
        BibliographicClaim {
            title: None,
            year: Some(1989),
            journal: Some("J. Phys. Chem.".into()),
            volume: Some("93".into()),
            issue: None,
            pages: Some("7009".into()),
            authors: Some(vec!["Voth".into(), "Chandler".into(), "Miller".into()]),
        }
    }

    #[test]
    fn existence_only_stays_valid() {
        let line = format_verify_line("10.1021/j100134a002", &voth_1993(), None);
        assert_eq!(
            line,
            "VALID 10.1021/j100134a002 : Feynman path integral formulation of quantum mechanical transition-state theory"
        );
    }

    #[test]
    fn voth_1989_claim_on_the_1993_review_is_a_mismatch() {
        let line = format_verify_line("10.1021/j100134a002", &voth_1993(), Some(&claim_1989_jpc()));
        assert!(line.starts_with("MISMATCH "), "{line}");
        assert!(!line.contains("VALID"), "{line}");
        assert!(line.contains("year: claimed 1989, record 1993"), "{line}");
        assert!(line.contains("volume: claimed 93, record 97"), "{line}");
        assert!(
            line.contains("pages: claimed 7009, record 8365-8377"),
            "{line}"
        );
        assert!(line.contains("Chandler"), "{line}");
        assert!(!line.contains("journal:"), "{line}");
    }

    #[test]
    fn matching_jcp_record_agrees_including_abbreviated_journal_and_first_page() {
        let meta = json!({
            "doi": "10.1063/1.457242",
            "title": "Rigorous formulation of quantum transition state theory and its dynamical corrections",
            "authors": [
                {"family": "Voth"},
                {"family": "Chandler"},
                {"family": "Miller"}
            ],
            "date": {"year": 1989},
            "journal": "The Journal of Chemical Physics",
            "volume": "091",
            "issue": "12",
            "pages": "7749-7760"
        });
        let claim = BibliographicClaim {
            title: Some("Rigorous formulation of quantum transition state theory".into()),
            year: Some(1989),
            journal: Some("J. Chem. Phys.".into()),
            volume: Some("91".into()),
            issue: Some("12".into()),
            pages: Some("7749".into()),
            authors: Some(vec!["Gregory A. Voth".into(), "Chandler, David".into()]),
        };
        let line = format_verify_line("10.1063/1.457242", &meta, Some(&claim));
        assert!(line.starts_with("VALID "), "{line}");
    }

    #[test]
    fn journal_of_physical_chemistry_does_not_agree_with_chemical_physics() {
        let meta = json!({
            "journal": "The Journal of Chemical Physics",
            "date": {"year": 1989},
            "title": "Rigorous formulation of quantum transition state theory and its dynamical corrections",
            "authors": [{"family": "Voth"}]
        });
        let claim = BibliographicClaim {
            journal: Some("J. Phys. Chem.".into()),
            ..BibliographicClaim::default()
        };
        let line = format_verify_line("10.1063/1.457242", &meta, Some(&claim));
        assert!(line.starts_with("MISMATCH "), "{line}");
        assert!(line.contains("journal:"), "{line}");
    }

    #[test]
    fn a_missing_volume_is_incomplete_when_nothing_else_disagrees() {
        let meta = json!({
            "title": "Feynman path integral formulation of quantum mechanical transition-state theory",
            "authors": [{"family": "Voth"}],
            "date": {"year": 1993},
            "journal": "The Journal of Physical Chemistry"
        });
        let claim = BibliographicClaim {
            year: Some(1993),
            volume: Some("97".into()),
            ..BibliographicClaim::default()
        };
        let line = format_verify_line("10.1021/j100134a002", &meta, Some(&claim));
        assert!(line.starts_with("INCOMPLETE "), "{line}");
        assert!(!line.contains("VALID"), "{line}");
        assert!(
            line.contains("volume: claimed 97, record has none"),
            "{line}"
        );
    }

    #[test]
    fn year_disagreement_wins_over_a_missing_volume() {
        let meta = json!({
            "title": "Feynman path integral formulation of quantum mechanical transition-state theory",
            "date": {"year": 1993},
            "journal": "The Journal of Physical Chemistry",
            "authors": [{"family": "Voth"}]
        });
        let claim = BibliographicClaim {
            year: Some(1989),
            volume: Some("93".into()),
            ..BibliographicClaim::default()
        };
        let line = format_verify_line("10.1021/j100134a002", &meta, Some(&claim));
        assert!(line.starts_with("MISMATCH "), "{line}");
        assert!(line.contains("year: claimed 1989, record 1993"), "{line}");
        assert!(line.contains("record has none"), "{line}");
    }

    #[test]
    fn the_1989_z_factor_title_does_not_agree_with_the_1993_review() {
        let claim = BibliographicClaim {
            title: Some(
                "Time correlation function and path integral analysis of quantum rate constants"
                    .into(),
            ),
            ..BibliographicClaim::default()
        };
        let line = format_verify_line("10.1021/j100134a002", &voth_1993(), Some(&claim));
        assert!(line.starts_with("MISMATCH "), "{line}");
        assert!(line.contains("title:"), "{line}");
    }

    #[test]
    fn validate_doi_prints_pages_and_a_mismatch_head() {
        let text = format_validate_doi("10.1021/j100134a002", &voth_1993(), &claim_1989_jpc());
        assert!(text.starts_with("MISMATCH\n"), "{text}");
        assert!(text.contains("Pages: 8365-8377"), "{text}");
        assert!(!text.contains("VALID"), "{text}");
    }
}
