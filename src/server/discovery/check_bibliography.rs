//! `check_bibliography`: a pasted BibTeX or RIS bibliography, checked for
//! what a reference manager hides.
//!
//! For each entry the tool reports a resolved DOI (the one written, or one
//! found from the title when it is missing and the match is strict), a
//! retraction, withdrawal, or expression of concern read from the publisher
//! record, and, for a preprint, the published version the publisher
//! records. Across entries it groups duplicates. Field-by-field comparison
//! with the record stays with `audit_bibliography`.

use std::collections::{BTreeMap, HashMap, HashSet};

use futures::{stream, StreamExt};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use crate::batch_limits::read_only_concurrency;
use crate::bib_audit::{
    audit_quota_refusal, notices_from_provider, parse_bibtex, AuditQuota, BibEntry, DoiOutcome,
    NoticeKind, PublisherNotice,
};
use crate::constants::{
    ANON_BATCH_SOFT_CAP, ANON_DAILY_LOOKUPS, AUDIT_BIB_MAX_BYTES, AUDIT_BIB_MAX_ENTRIES,
    FREE_DAILY_LOOKUPS,
};
use crate::server::Server;

/// A title found for an entry with no DOI must agree this closely with the
/// entry's own title, as the Jaccard overlap of their word sets.
const TITLE_MATCH_THRESHOLD: f64 = 0.85;
/// A title shorter than this, in letters and digits, is too short to call
/// two entries duplicates on.
const MIN_DUPLICATE_TITLE_CHARS: usize = 12;

/// OokCite calls one title lookup makes: resolve, reverse, and the lookup of
/// the DOI they find. The quota check counts all three.
const TITLE_LOOKUP_COST: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BibFormat {
    /// Decide from the text: RIS when it has `TY  -` records, BibTeX otherwise.
    #[default]
    Auto,
    Bibtex,
    Ris,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckBibliographyArgs {
    /// The bibliography as text: BibTeX (`@article{...}`) or RIS (`TY  - JOUR` records). Up to 1 MiB and 2000 entries.
    #[schemars(length(min = 5, max = 1048576))]
    pub content: String,
    /// `bibtex`, `ris`, or `auto` (default), which reads RIS when the text has `TY  -` records.
    #[serde(default)]
    pub format: BibFormat,
    /// For an entry with no DOI, look one up from its title, first author, and year, and accept it only when the title agrees closely and the year does not conflict (default true). Each lookup counts against the daily quota.
    #[serde(default = "default_true")]
    pub resolve_missing_dois: bool,
}

// ---------------------------------------------------------------- RIS

struct RisRecord {
    fields: Vec<(String, String)>,
}

fn ris_tag(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start_matches('\u{feff}');
    let bytes = line.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    let tag = line.get(..2)?;
    let mut chars = tag.chars();
    let first = chars.next()?;
    let second = chars.next()?;
    if !first.is_ascii_uppercase() || !(second.is_ascii_uppercase() || second.is_ascii_digit()) {
        return None;
    }
    let rest = line.get(2..)?.trim_start();
    let value = rest.strip_prefix('-')?;
    Some((tag, value.trim()))
}

fn ris_entry_type(raw: &str) -> &'static str {
    match raw.trim().to_ascii_uppercase().as_str() {
        "JOUR" | "JFULL" | "MGZN" | "NEWS" => "article",
        "CONF" | "CPAPER" => "inproceedings",
        "BOOK" | "EBOOK" => "book",
        "CHAP" | "ECHAP" => "incollection",
        "RPRT" => "techreport",
        "THES" => "phdthesis",
        "UNPB" | "INPR" | "PREPRINT" => "unpublished",
        _ => "misc",
    }
}

fn first_year(raw: &str) -> Option<i32> {
    let digits: Vec<char> = raw.chars().collect();
    digits
        .windows(4)
        .find(|window| window.iter().all(char::is_ascii_digit))
        .and_then(|window| window.iter().collect::<String>().parse().ok())
}

fn record_to_entry(record: &RisRecord, ordinal: usize) -> BibEntry {
    let first = |tags: &[&str]| -> Option<String> {
        tags.iter().find_map(|wanted| {
            record
                .fields
                .iter()
                .find(|(tag, value)| tag == wanted && !value.is_empty())
                .map(|(_, value)| value.clone())
        })
    };
    let entry_type = ris_entry_type(&first(&["TY"]).unwrap_or_default());
    let authors: Vec<String> = record
        .fields
        .iter()
        .filter(|(tag, value)| matches!(tag.as_str(), "AU" | "A1") && !value.is_empty())
        .map(|(_, value)| value.clone())
        .collect();
    let pages = match (first(&["SP"]), first(&["EP"])) {
        (Some(start), Some(end)) => Some(format!("{start}-{end}")),
        (Some(start), None) => Some(start),
        _ => None,
    };
    let raw_doi = first(&["DO"]).or_else(|| first(&["UR", "L3"]));
    let (doi, malformed_doi) = match raw_doi {
        Some(raw) => match crate::bib_audit::clean_doi(&raw) {
            Some(doi) => (Some(doi), None),
            None if first(&["DO"]).is_some() => (None, Some(raw)),
            None => (None, None),
        },
        None => (None, None),
    };
    BibEntry {
        key: first(&["ID"]).unwrap_or_else(|| format!("ris{ordinal}")),
        entry_type: entry_type.to_string(),
        title: first(&["TI", "T1"]),
        authors,
        year: first(&["PY", "Y1", "DA"]).and_then(|raw| first_year(&raw)),
        journal: first(&["JO", "JF", "T2", "JA"]),
        volume: first(&["VL"]),
        issue: first(&["IS"]),
        pages,
        doi,
        malformed_doi,
    }
}

pub(super) fn parse_ris(content: &str) -> Result<Vec<BibEntry>, String> {
    if content.len() > AUDIT_BIB_MAX_BYTES {
        return Err(format!(
            "bibliography is {} bytes; the limit is {AUDIT_BIB_MAX_BYTES}",
            content.len()
        ));
    }
    let mut entries = Vec::new();
    let mut current: Option<RisRecord> = None;
    let finish = |record: Option<RisRecord>, entries: &mut Vec<BibEntry>| {
        if let Some(record) = record {
            let ordinal = entries.len() + 1;
            entries.push(record_to_entry(&record, ordinal));
        }
    };
    for line in content.lines() {
        match ris_tag(line) {
            Some(("TY", value)) => {
                finish(current.take(), &mut entries);
                current = Some(RisRecord {
                    fields: vec![("TY".to_string(), value.to_string())],
                });
            }
            Some(("ER", _)) => finish(current.take(), &mut entries),
            Some((tag, value)) => {
                if let Some(record) = current.as_mut() {
                    record.fields.push((tag.to_string(), value.to_string()));
                }
            }
            None => {
                let text = line.trim();
                if let (false, Some(record)) = (text.is_empty(), current.as_mut()) {
                    if let Some((_, value)) = record.fields.last_mut() {
                        value.push(' ');
                        value.push_str(text);
                    }
                }
            }
        }
        if entries.len() > AUDIT_BIB_MAX_ENTRIES {
            return Err(format!(
                "bibliography has more than {AUDIT_BIB_MAX_ENTRIES} entries"
            ));
        }
    }
    finish(current.take(), &mut entries);
    Ok(entries)
}

// ------------------------------------------------------ text matching

fn words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub(super) fn title_overlap(left: &str, right: &str) -> f64 {
    let left: HashSet<String> = words(left).into_iter().collect();
    let right: HashSet<String> = words(right).into_iter().collect();
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let common = left.intersection(&right).count() as f64;
    let union = left.union(&right).count() as f64;
    common / union
}

fn family_name(author: &str) -> String {
    let author = author.trim();
    match author.split_once(',') {
        Some((family, _)) => family.trim().to_lowercase(),
        None => author
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_lowercase(),
    }
}

fn title_key(title: &str) -> String {
    words(title).join("")
}

// ----------------------------------------------- publisher record facts

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreprintFacts {
    /// DOIs the publisher records as the published version.
    pub(super) published: Vec<String>,
    /// Which record said this entry is a preprint.
    pub(super) basis: &'static str,
}

pub(super) fn preprint_facts(provider: &Value, doi: &str) -> Option<PreprintFacts> {
    let mut published = Vec::new();
    let mut basis = None;
    if let Some(message) = provider.get("crossref").filter(|value| value.is_object()) {
        if message["type"].as_str() == Some("posted-content") {
            basis = Some("Crossref type posted-content");
        }
        if let Some(items) = message["relation"]["is-preprint-of"].as_array() {
            basis = basis.or(Some("Crossref is-preprint-of"));
            for item in items {
                let id = item["id"].as_str().unwrap_or("");
                let id_type = item["id-type"].as_str().unwrap_or("doi");
                if id_type.eq_ignore_ascii_case("doi") {
                    if let Some(found) = crate::bib_audit::clean_doi(id) {
                        published.push(found);
                    }
                }
            }
        }
    }
    if let Some(attrs) = provider.get("datacite").filter(|value| value.is_object()) {
        if attrs["types"]["resourceTypeGeneral"].as_str() == Some("Preprint") {
            basis = Some("DataCite resource type Preprint");
        }
        for relation in attrs["relatedIdentifiers"].as_array().into_iter().flatten() {
            let kind = relation["relationType"].as_str().unwrap_or("");
            let id_type = relation["relatedIdentifierType"].as_str().unwrap_or("");
            if kind == "IsPreprintOf" && id_type.eq_ignore_ascii_case("doi") {
                basis = basis.or(Some("DataCite IsPreprintOf"));
                if let Some(found) = relation["relatedIdentifier"]
                    .as_str()
                    .and_then(crate::bib_audit::clean_doi)
                {
                    published.push(found);
                }
            }
        }
    }
    if basis.is_none() && doi.starts_with("10.48550/arxiv.") {
        basis = Some("arXiv DOI prefix");
    }
    published.sort();
    published.dedup();
    published.retain(|candidate| candidate != doi);
    basis.map(|basis| PreprintFacts { published, basis })
}

#[derive(Debug, Clone)]
pub(super) struct DoiFacts {
    pub(super) notices: Vec<PublisherNotice>,
    pub(super) preprint: Option<PreprintFacts>,
    /// The publisher record could not be read, so notices and preprint
    /// status are unknown. Holds the reason.
    pub(super) unread: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) enum DoiState {
    Broken(String),
    Upstream(String),
    Read(DoiFacts),
}

pub(super) fn state_from_outcome(doi: &str, outcome: &DoiOutcome) -> DoiState {
    match outcome {
        DoiOutcome::Broken(reason) => DoiState::Broken(reason.clone()),
        DoiOutcome::Upstream(error) => DoiState::Upstream(first_line(error)),
        DoiOutcome::Unchecked(reason) => DoiState::Read(DoiFacts {
            notices: Vec::new(),
            preprint: None,
            unread: Some(first_line(reason)),
        }),
        DoiOutcome::Resolved {
            provider,
            provider_error,
            ..
        } => {
            if let Some(error) = provider_error
                .as_deref()
                .filter(|error| crate::http_error::is_stopping_failure(error))
            {
                return DoiState::Upstream(first_line(error));
            }
            let unread = match (provider, provider_error) {
                (Some(_), None) => None,
                (_, Some(error)) => Some(first_line(error)),
                (None, None) => Some("no Crossref or DataCite record was read".to_string()),
            };
            DoiState::Read(DoiFacts {
                notices: notices_from_provider(provider.as_ref()),
                preprint: provider
                    .as_ref()
                    .and_then(|record| preprint_facts(record, doi)),
                unread,
            })
        }
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

// ----------------------------------------------------- DOI resolution

#[derive(Debug, Clone)]
pub(super) enum Found {
    Given(String),
    FromTitle { doi: String, overlap: f64 },
    Rejected(String),
    Missing(&'static str),
    Failed(String),
}

impl Found {
    pub(super) fn doi(&self) -> Option<&str> {
        match self {
            Self::Given(doi) | Self::FromTitle { doi, .. } => Some(doi),
            _ => None,
        }
    }
}

/// Accept a corpus answer for an entry that has no DOI only when it names
/// the same paper: a close title, a year that does not conflict, and a
/// first author the answer also lists.
pub(super) fn accept_title_match(entry: &BibEntry, answer: &Value) -> Found {
    let Some(doi) = answer["doi"].as_str().and_then(crate::bib_audit::clean_doi) else {
        return Found::Missing("the lookup answered without a DOI");
    };
    let title = entry.title.as_deref().unwrap_or("");
    let found_title = answer["title"].as_str().unwrap_or("");
    let overlap = title_overlap(title, found_title);
    if overlap < TITLE_MATCH_THRESHOLD {
        return Found::Rejected(format!(
            "closest record doi:{doi} has a different title ({:.0}% word overlap)",
            overlap * 100.0
        ));
    }
    if let (Some(year), Some(found_year)) = (entry.year, answer["date"]["year"].as_i64()) {
        if i64::from(year) != found_year {
            return Found::Rejected(format!(
                "closest record doi:{doi} is from {found_year}, the entry says {year}"
            ));
        }
    }
    if let (Some(first), Some(listed)) = (entry.authors.first(), answer["authors"].as_array()) {
        let wanted = family_name(first);
        let families: Vec<String> = listed
            .iter()
            .filter_map(|author| author["family"].as_str())
            .map(str::to_lowercase)
            .collect();
        if !wanted.is_empty() && !families.is_empty() && !families.contains(&wanted) {
            return Found::Rejected(format!(
                "closest record doi:{doi} does not list {wanted} among its authors"
            ));
        }
    }
    Found::FromTitle { doi, overlap }
}

fn resolution_query(entry: &BibEntry) -> String {
    let mut parts = vec![entry.title.clone().unwrap_or_default()];
    if let Some(author) = entry.authors.first() {
        parts.push(family_name(author));
    }
    if let Some(year) = entry.year {
        parts.push(year.to_string());
    }
    parts.join(" ")
}

// ------------------------------------------------------------- report

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Broken,
    Retracted,
    Withdrawn,
    Concern,
    Upstream,
    Upgrade,
    Preprint,
    Unchecked,
    NoDoi,
    Ok,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Broken => "BROKEN",
            Self::Retracted => "RETRACTED",
            Self::Withdrawn => "WITHDRAWN",
            Self::Concern => "CONCERN",
            Self::Upstream => "UPSTREAM",
            Self::Upgrade => "UPGRADE",
            Self::Preprint => "PREPRINT",
            Self::Unchecked => "UNCHECKED",
            Self::NoDoi => "NO DOI",
            Self::Ok => "OK",
        }
    }
}

struct Row {
    key: String,
    entry_type: String,
    doi: Option<String>,
    status: Status,
    notes: Vec<String>,
}

fn notice_status(kind: NoticeKind) -> Option<Status> {
    match kind {
        NoticeKind::Retraction => Some(Status::Retracted),
        NoticeKind::Withdrawal => Some(Status::Withdrawn),
        NoticeKind::Concern => Some(Status::Concern),
        NoticeKind::Correction => None,
    }
}

fn row_for(entry: &BibEntry, found: &Found, states: &HashMap<String, DoiState>) -> Row {
    let mut row = Row {
        key: entry.key.clone(),
        entry_type: entry.entry_type.clone(),
        doi: found.doi().map(str::to_string),
        status: Status::Ok,
        notes: Vec::new(),
    };
    if let Some(bad) = &entry.malformed_doi {
        row.status = Status::Broken;
        row.notes
            .push(format!("the doi field `{bad}` is not a DOI"));
        return row;
    }
    match found {
        Found::FromTitle { doi, overlap } => row.notes.push(format!(
            "DOI found from the title: doi:{doi} (title overlap {:.0}%, year and first author do not conflict). Add it to the entry.",
            overlap * 100.0
        )),
        Found::Rejected(why) => {
            row.status = Status::NoDoi;
            row.notes
                .push(format!("no DOI; not guessed: {why}"));
            return row;
        }
        Found::Missing(why) => {
            row.status = Status::NoDoi;
            row.notes.push(format!("no DOI; {why}"));
            return row;
        }
        Found::Failed(error) => {
            row.status = Status::Upstream;
            row.notes.push(format!("DOI lookup failed: {error}"));
            return row;
        }
        Found::Given(_) => {}
    }
    let Some(doi) = found.doi() else {
        return row;
    };
    match states.get(doi) {
        None => {
            row.status = Status::Unchecked;
            row.notes.push("not looked up".into());
        }
        Some(DoiState::Broken(reason)) => {
            row.status = Status::Broken;
            row.notes.push(reason.clone());
        }
        Some(DoiState::Upstream(error)) => {
            row.status = Status::Upstream;
            row.notes.push(error.clone());
        }
        Some(DoiState::Read(facts)) => {
            let mut worst: Option<(Status, &PublisherNotice)> = None;
            for notice in &facts.notices {
                if let Some(status) = notice_status(notice.kind) {
                    row.notes.push(notice.line());
                    if worst.is_none_or(|(current, _)| status < current) {
                        worst = Some((status, notice));
                    }
                }
            }
            if let Some((status, _)) = worst {
                row.status = status;
            }
            if let Some(preprint) = &facts.preprint {
                match preprint.published.as_slice() {
                    [] => {
                        row.notes.push(format!(
                            "preprint ({}); the publisher records no published version",
                            preprint.basis
                        ));
                        if row.status == Status::Ok {
                            row.status = Status::Preprint;
                        }
                    }
                    versions => {
                        let listed = versions
                            .iter()
                            .map(|doi| format!("doi:{doi}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        row.notes.push(format!(
                            "preprint ({}); published version {listed}. Cite the published version.",
                            preprint.basis
                        ));
                        if row.status == Status::Ok {
                            row.status = Status::Upgrade;
                        }
                    }
                }
            }
            if let Some(reason) = &facts.unread {
                row.notes.push(format!("not checked: {reason}"));
                if row.status == Status::Ok {
                    row.status = Status::Unchecked;
                }
            }
        }
    }
    row
}

/// Groups of entry indexes that are the same work, with the reason. Two
/// entries are the same work when they share a DOI, when their titles are
/// identical and their years and first authors do not conflict, or when
/// one is a preprint whose published version the other cites.
pub(super) fn duplicate_groups(
    entries: &[BibEntry],
    dois: &[Option<String>],
    states: &HashMap<String, DoiState>,
) -> Vec<(Vec<usize>, String)> {
    let count = entries.len();
    let mut parent: Vec<usize> = (0..count).collect();
    let mut reasons: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    fn root(parent: &mut [usize], mut node: usize) -> usize {
        while parent[node] != node {
            parent[node] = parent[parent[node]];
            node = parent[node];
        }
        node
    }
    let mut join = |parent: &mut Vec<usize>, a: usize, b: usize, reason: String| {
        let (ra, rb) = (root(parent, a), root(parent, b));
        if ra != rb {
            parent[rb] = ra;
        }
        let top = root(parent, a);
        let list = reasons.entry(top).or_default();
        if !list.contains(&reason) {
            list.push(reason);
        }
    };
    let mut by_doi: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, doi) in dois.iter().enumerate() {
        if let Some(doi) = doi {
            by_doi.entry(doi.as_str()).or_default().push(index);
        }
    }
    for (doi, members) in &by_doi {
        for pair in members.windows(2) {
            join(&mut parent, pair[0], pair[1], format!("same DOI doi:{doi}"));
        }
    }
    let keys: Vec<String> = entries
        .iter()
        .map(|entry| title_key(entry.title.as_deref().unwrap_or("")))
        .collect();
    for left in 0..count {
        if keys[left].chars().count() < MIN_DUPLICATE_TITLE_CHARS {
            continue;
        }
        for right in (left + 1)..count {
            if keys[left] != keys[right] {
                continue;
            }
            let year_conflict = matches!(
                (entries[left].year, entries[right].year),
                (Some(a), Some(b)) if a != b
            );
            let author_conflict = match (
                entries[left].authors.first(),
                entries[right].authors.first(),
            ) {
                (Some(a), Some(b)) => family_name(a) != family_name(b),
                _ => false,
            };
            if !year_conflict && !author_conflict {
                join(&mut parent, left, right, "same title".to_string());
            }
        }
    }
    for (index, doi) in dois.iter().enumerate() {
        let Some(DoiState::Read(facts)) = doi.as_deref().and_then(|doi| states.get(doi)) else {
            continue;
        };
        let Some(preprint) = &facts.preprint else {
            continue;
        };
        for published in &preprint.published {
            for other in by_doi.get(published.as_str()).into_iter().flatten() {
                if *other != index {
                    join(
                        &mut parent,
                        index,
                        *other,
                        format!(
                            "preprint doi:{} and its published version doi:{published}",
                            doi.as_deref().unwrap_or("")
                        ),
                    );
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..count {
        let top = root(&mut parent, index);
        groups.entry(top).or_default().push(index);
    }
    let mut out = Vec::new();
    for (top, members) in groups {
        if members.len() < 2 {
            continue;
        }
        let why = reasons
            .get(&top)
            .map(|list| list.join("; "))
            .unwrap_or_default();
        out.push((members, why));
    }
    out.sort_by_key(|(members, _)| members[0]);
    out
}

pub(super) fn render_check(
    format_label: &str,
    entries: &[BibEntry],
    founds: &[Found],
    states: &HashMap<String, DoiState>,
) -> String {
    let dois: Vec<Option<String>> = founds
        .iter()
        .map(|found| found.doi().map(str::to_string))
        .collect();
    let rows: Vec<Row> = entries
        .iter()
        .zip(founds)
        .map(|(entry, found)| row_for(entry, found, states))
        .collect();
    let groups = duplicate_groups(entries, &dois, states);

    let with_doi = founds
        .iter()
        .filter(|found| matches!(found, Found::Given(_)))
        .count();
    let from_title = founds
        .iter()
        .filter(|found| matches!(found, Found::FromTitle { .. }))
        .count();
    let mut counts: BTreeMap<Status, usize> = BTreeMap::new();
    for row in &rows {
        *counts.entry(row.status).or_default() += 1;
    }
    let summary = counts
        .iter()
        .map(|(status, count)| format!("{count} {}", status.label()))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = vec![
        format!(
            "BIBLIOGRAPHY CHECK ({format_label}): {} entries, {with_doi} with a DOI, {from_title} DOI(s) found from titles",
            entries.len()
        ),
        format!(
            "Summary: {}; {} duplicate group(s)",
            if summary.is_empty() { "no entries".to_string() } else { summary },
            groups.len()
        ),
    ];
    for row in &rows {
        let doi = row
            .doi
            .as_deref()
            .map(|doi| format!(" doi:{doi}"))
            .unwrap_or_default();
        out.push(format!(
            "[{}] {}{doi} ({})",
            row.key,
            row.status.label(),
            row.entry_type
        ));
        for note in &row.notes {
            out.push(format!("    {note}"));
        }
    }
    if !groups.is_empty() {
        out.push("Duplicates:".into());
        for (members, why) in &groups {
            let keys = members
                .iter()
                .map(|index| format!("[{}]", entries[*index].key))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!("- {keys}: {why}"));
        }
    }
    out.push(
        "Metadata only. This does not compare titles, authors, or pages with the record: run audit_bibliography on the corrected .bib for that."
            .into(),
    );
    out.join("\n")
}

// --------------------------------------------------------------- tool

impl Server {
    async fn find_doi_for_entry(&self, entry: &BibEntry) -> Found {
        let Some(title) = entry.title.as_deref().filter(|t| !t.trim().is_empty()) else {
            return Found::Missing("the entry has no title to search with");
        };
        if words(title).len() < 3 {
            return Found::Missing("the title is too short to search with");
        }
        match self
            .resolve_query_to_metadata(&resolution_query(entry), false)
            .await
        {
            Ok(Some(answer)) => accept_title_match(entry, &answer),
            Ok(None) => Found::Missing("no record matches the title"),
            Err(error) => Found::Failed(first_line(&error)),
        }
    }
}

#[tool_router(router = check_bibliography_tool_router, vis = "pub(super)")]
impl Server {
    #[tool(
        name = "check_bibliography",
        description = "Check a pasted BibTeX or RIS bibliography for what a reference manager hides. For each entry it reports the DOI (the one written, or one found from the title when it is missing and the title, year, and first author agree; a weak match is not guessed), a retraction, withdrawal, or expression of concern read from the Crossref or DataCite record, and, for a preprint, the published version the publisher records. It also groups duplicates: the same DOI, the same title, or a preprint next to its published version. Call this on a bibliography the user pasted, as text, before submission or before merging two lists. For a manuscript .bib compared field by field with the record (title, authors, pages) use audit_bibliography; for a collection use check_duplicates. RIS is read here and not in audit_bibliography. Every DOI looked up counts against the daily quota, and a large paste is refused up front when it cannot fit. A 429 or 402 is reported with its Retry-After and the entry is UPSTREAM, never clean.",
        annotations(
            title = "Check bibliography",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn check_bibliography(
        &self,
        Parameters(args): Parameters<CheckBibliographyArgs>,
    ) -> String {
        let (format_label, parsed) = match args.format {
            BibFormat::Bibtex => ("BibTeX", parse_bibtex(&args.content)),
            BibFormat::Ris => ("RIS", parse_ris(&args.content)),
            BibFormat::Auto if crate::plaintext::looks_like_ris(&args.content) => {
                ("RIS", parse_ris(&args.content))
            }
            BibFormat::Auto => ("BibTeX", parse_bibtex(&args.content)),
        };
        let entries = match parsed {
            Ok(entries) => entries,
            Err(error) => return format!("CLIENT ERROR: {format_label} parse error: {error}"),
        };
        if entries.is_empty() {
            return format!("CLIENT ERROR: no {format_label} entries found in the text.");
        }

        let mut seen = HashSet::new();
        let given_unique = entries
            .iter()
            .filter(|entry| entry.malformed_doi.is_none())
            .filter_map(|entry| entry.doi.clone())
            .filter(|doi| seen.insert(doi.clone()))
            .count();
        let to_resolve: Vec<usize> = if args.resolve_missing_dois {
            entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.doi.is_none() && entry.malformed_doi.is_none())
                .map(|(index, _)| index)
                .collect()
        } else {
            Vec::new()
        };

        let access = if given_unique + to_resolve.len() == 0 {
            crate::server::AuditAccess::anonymous()
        } else {
            self.audit_access().await
        };
        if let Some(message) = access.rejected {
            return message;
        }
        let metered =
            u32::try_from(given_unique + to_resolve.len() * TITLE_LOOKUP_COST).unwrap_or(u32::MAX);
        if let Some(message) = audit_quota_refusal(&AuditQuota {
            metered,
            has_api_key: access.has_key,
            remaining: access.remaining,
            limit: access.limit,
            plan: access.plan,
            anon_soft_cap: ANON_BATCH_SOFT_CAP,
            anon_daily: ANON_DAILY_LOOKUPS,
            free_daily: FREE_DAILY_LOOKUPS,
        }) {
            return message;
        }
        if access.has_key && !access.checked {
            return "REFUSED: the API key could not be checked, so this check did not call doi.org."
                .into();
        }

        let conc = read_only_concurrency();
        let lookups: Vec<_> = to_resolve
            .iter()
            .map(|index| {
                let server = self.clone();
                let entry = entries[*index].clone();
                let index = *index;
                async move { (index, server.find_doi_for_entry(&entry).await) }
            })
            .collect();
        let resolved: Vec<(usize, Found)> = stream::iter(lookups).buffered(conc).collect().await;
        let mut resolved: HashMap<usize, Found> = resolved.into_iter().collect();

        let founds: Vec<Found> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                if entry.malformed_doi.is_some() {
                    return Found::Missing("the doi field is not a DOI");
                }
                match &entry.doi {
                    Some(doi) => Found::Given(doi.clone()),
                    None => resolved.remove(&index).unwrap_or(Found::Missing(
                        "resolve_missing_dois is false, so no lookup was made",
                    )),
                }
            })
            .collect();

        let mut unique: Vec<String> = Vec::new();
        let mut once = HashSet::new();
        for found in &founds {
            if let Some(doi) = found.doi() {
                if once.insert(doi.to_string()) {
                    unique.push(doi.to_string());
                }
            }
        }
        let inspections: Vec<_> = unique
            .into_iter()
            .map(|doi| {
                let server = self.clone();
                async move {
                    let outcome = server.inspect_doi(&doi).await;
                    (doi, outcome)
                }
            })
            .collect();
        let outcomes: Vec<(String, DoiOutcome)> =
            stream::iter(inspections).buffered(conc).collect().await;
        let states: HashMap<String, DoiState> = outcomes
            .iter()
            .map(|(doi, outcome)| (doi.clone(), state_from_outcome(doi, outcome)))
            .collect();

        render_check(format_label, &entries, &founds, &states)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::EnvGuard;
    use rmcp::model::{CallToolResult, ContentBlock};
    use rmcp::ServerHandler;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn server(mock: &MockServer) -> Server {
        Server::new_with_base(mock.uri()).with_providers(mock.uri())
    }

    fn args(content: &str) -> CheckBibliographyArgs {
        CheckBibliographyArgs {
            content: content.into(),
            format: BibFormat::Auto,
            resolve_missing_dois: true,
        }
    }

    fn enc(doi: &str) -> String {
        crate::server::encode_path_segment(doi)
    }

    /// A DOI the registry, OokCite, and Crossref (or DataCite) all know.
    async fn mount_doi(
        mock: &MockServer,
        doi: &str,
        crossref: Option<Value>,
        datacite: Option<Value>,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/handles/{}", enc(doi))))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": 1})))
            .mount(mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .and(body_string_contains(doi))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "doi": doi,
                "title": "A mock record"
            })))
            .mount(mock)
            .await;
        let crossref_response = match crossref {
            Some(message) => ResponseTemplate::new(200)
                .set_body_json(json!({"status": "ok", "message": message})),
            None => ResponseTemplate::new(404),
        };
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc(doi))))
            .respond_with(crossref_response)
            .mount(mock)
            .await;
        if let Some(attributes) = datacite {
            Mock::given(method("GET"))
                .and(path(format!("/dois/{}", enc(doi))))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": {"attributes": attributes}})),
                )
                .mount(mock)
                .await;
        }
    }

    const BIB: &str = r#"
@article{clean2020,
  title = {Crystal nucleation in sheared colloidal glasses},
  author = {Ada Example and Ben Sample},
  year = {2020},
  doi = {10.5555/clean.2020}
}
@article{clean2020b,
  title = {Crystal nucleation in sheared colloidal glasses (extended)},
  year = {2020},
  doi = {10.5555/clean.2020}
}
@article{retracted2019,
  title = {A claim that did not survive},
  year = {2019},
  doi = {10.5555/retracted.2019}
}
@misc{preprint2021,
  title = {Shear-induced ordering, a preprint},
  year = {2021},
  doi = {10.5555/preprint.2021}
}
@article{published2021,
  title = {Shear-induced ordering},
  year = {2021},
  doi = {10.5555/pub.2021}
}
@misc{arxiv2022,
  title = {An arXiv-only study},
  year = {2022},
  doi = {10.48550/arXiv.2201.00001}
}
@article{nodoi2018,
  title = {Fast crystallisation of sheared colloidal suspensions},
  author = {Smith, Jo},
  year = {2018}
}
@article{ghost2017,
  title = {An entirely unknown manuscript about nothing},
  author = {Doe, Jane},
  year = {2017}
}
"#;

    async fn mount_bib_world(mock: &MockServer) {
        let plain = |doi: &str| json!({"DOI": doi, "type": "journal-article"});
        mount_doi(
            mock,
            "10.5555/clean.2020",
            Some(plain("10.5555/clean.2020")),
            None,
        )
        .await;
        mount_doi(
            mock,
            "10.5555/retracted.2019",
            Some(json!({
                "DOI": "10.5555/retracted.2019",
                "update-to": [{"type": "retraction", "DOI": "10.5555/notice.2020", "updated": {"date-parts": [[2020, 6]]}}]
            })),
            None,
        )
        .await;
        mount_doi(
            mock,
            "10.5555/preprint.2021",
            Some(json!({
                "DOI": "10.5555/preprint.2021",
                "type": "posted-content",
                "subtype": "preprint",
                "relation": {"is-preprint-of": [{"id-type": "doi", "id": "10.5555/pub.2021"}]}
            })),
            None,
        )
        .await;
        mount_doi(
            mock,
            "10.5555/pub.2021",
            Some(plain("10.5555/pub.2021")),
            None,
        )
        .await;
        mount_doi(
            mock,
            "10.48550/arxiv.2201.00001",
            None,
            Some(json!({
                "types": {"resourceTypeGeneral": "Preprint"},
                "relatedIdentifiers": [
                    {"relationType": "IsPreprintOf", "relatedIdentifierType": "DOI", "relatedIdentifier": "10.5555/pub.2022"}
                ]
            })),
        )
        .await;
        mount_doi(
            mock,
            "10.5555/found.2018",
            Some(plain("10.5555/found.2018")),
            None,
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .and(body_string_contains("Fast crystallisation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"paper": {
                "doi": "10.5555/found.2018",
                "title": "Fast crystallisation of sheared colloidal suspensions",
                "date": {"year": 2018},
                "authors": [{"family": "Smith"}, {"family": "Jones"}]
            }})))
            .mount(mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(mock)
            .await;
    }

    #[tokio::test]
    async fn a_bibtex_paste_reports_dois_retractions_upgrades_and_duplicates() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_bib_world(&mock).await;

        let text = server(&mock)
            .check_bibliography(Parameters(args(BIB)))
            .await;

        assert!(
            text.starts_with(
                "BIBLIOGRAPHY CHECK (BibTeX): 8 entries, 6 with a DOI, 1 DOI(s) found from titles"
            ),
            "{text}"
        );
        assert!(
            text.contains("[clean2020] OK doi:10.5555/clean.2020 (article)"),
            "{text}"
        );
        assert!(
            text.contains("[retracted2019] RETRACTED doi:10.5555/retracted.2019"),
            "{text}"
        );
        assert!(
            text.contains("    retracted by doi:10.5555/notice.2020 on 2020-06"),
            "{text}"
        );
        assert!(
            text.contains("[preprint2021] UPGRADE doi:10.5555/preprint.2021"),
            "{text}"
        );
        assert!(
            text.contains("preprint (Crossref type posted-content); published version doi:10.5555/pub.2021. Cite the published version."),
            "{text}"
        );
        assert!(
            text.contains("[arxiv2022] UPGRADE doi:10.48550/arxiv.2201.00001"),
            "{text}"
        );
        assert!(
            text.contains("published version doi:10.5555/pub.2022"),
            "{text}"
        );
        assert!(
            text.contains("[nodoi2018] OK doi:10.5555/found.2018"),
            "{text}"
        );
        assert!(
            text.contains("DOI found from the title: doi:10.5555/found.2018 (title overlap 100%"),
            "{text}"
        );
        assert!(text.contains("[ghost2017] NO DOI (article)"), "{text}");
        assert!(
            text.contains("no DOI; no record matches the title"),
            "{text}"
        );
        assert!(text.contains("Duplicates:"), "{text}");
        assert!(
            text.contains("- [clean2020], [clean2020b]: same DOI doi:10.5555/clean.2020"),
            "{text}"
        );
        assert!(
            text.contains("- [preprint2021], [published2021]: preprint doi:10.5555/preprint.2021 and its published version doi:10.5555/pub.2021"),
            "{text}"
        );
        assert!(text.contains("2 duplicate group(s)"), "{text}");
        assert!(
            crate::http_error::upstream_failure_report(&text).is_none(),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_ris_paste_is_read_and_checked() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_doi(
            &mock,
            "10.5555/retracted.2019",
            Some(json!({
                "DOI": "10.5555/retracted.2019",
                "update-to": [{"type": "retraction", "DOI": "10.5555/notice.2020"}]
            })),
            None,
        )
        .await;
        let ris = "TY  - JOUR\nID  - lee2019\nAU  - Lee, Kim\nTI  - A claim that did\n     not survive\nPY  - 2019///\nJO  - Journal of Mock Results\nDO  - 10.5555/retracted.2019\nER  - \n";

        let text = server(&mock)
            .check_bibliography(Parameters(args(ris)))
            .await;

        assert!(
            text.starts_with("BIBLIOGRAPHY CHECK (RIS): 1 entries, 1 with a DOI"),
            "{text}"
        );
        assert!(
            text.contains("[lee2019] RETRACTED doi:10.5555/retracted.2019 (article)"),
            "{text}"
        );
    }

    #[test]
    fn ris_records_become_entries_with_their_fields() {
        let ris = "\u{feff}TY  - JOUR\nID  - a1\nA1  - Ng, Ana\nAU  - Roe, Rae\nT1  - First title\nPY  - 2018/05/01\nJF  - Journal One\nVL  - 7\nIS  - 2\nSP  - 100\nEP  - 110\nUR  - https://doi.org/10.5555/AA.1\nER  - \n\nTY  - CONF\nTI  - Second title\nDA  - 2020\nDO  - not-a-doi\n";
        let entries = parse_ris(ris).unwrap();
        assert_eq!(entries.len(), 2);
        let first = &entries[0];
        assert_eq!(first.key, "a1");
        assert_eq!(first.entry_type, "article");
        assert_eq!(first.authors, ["Ng, Ana", "Roe, Rae"]);
        assert_eq!(first.title.as_deref(), Some("First title"));
        assert_eq!(first.year, Some(2018));
        assert_eq!(first.journal.as_deref(), Some("Journal One"));
        assert_eq!(
            (first.volume.as_deref(), first.issue.as_deref()),
            (Some("7"), Some("2"))
        );
        assert_eq!(first.pages.as_deref(), Some("100-110"));
        assert_eq!(first.doi.as_deref(), Some("10.5555/aa.1"));
        let second = &entries[1];
        assert_eq!(second.key, "ris2");
        assert_eq!(second.entry_type, "inproceedings");
        assert_eq!(second.year, Some(2020));
        assert_eq!(second.doi, None);
        assert_eq!(second.malformed_doi.as_deref(), Some("not-a-doi"));
    }

    #[tokio::test]
    async fn forcing_the_wrong_format_finds_no_entries() {
        let mock = MockServer::start().await;
        let mut request = args("TY  - JOUR\nTI  - Something\nER  - \n");
        request.format = BibFormat::Bibtex;

        let text = server(&mock).check_bibliography(Parameters(request)).await;

        assert_eq!(text, "CLIENT ERROR: no BibTeX entries found in the text.");
    }

    #[tokio::test]
    async fn a_different_title_is_not_accepted_as_the_missing_doi() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"paper": {
                "doi": "10.5555/other.1999",
                "title": "Completely unrelated work on weather",
                "date": {"year": 1999}
            }})))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock)
            .await;
        let bib = "@article{x, title={Fast crystallisation of sheared colloidal suspensions}, year={2018}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert!(text.contains("[x] NO DOI (article)"), "{text}");
        assert!(
            text.contains(
                "no DOI; not guessed: closest record doi:10.5555/other.1999 has a different title"
            ),
            "{text}"
        );
        assert!(!text.contains("DOI found from the title"), "{text}");
    }

    #[tokio::test]
    async fn resolve_missing_dois_false_makes_no_title_lookup() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;
        let mut request =
            args("@article{x, title={Fast crystallisation of sheared colloidal suspensions}}");
        request.resolve_missing_dois = false;

        let text = server(&mock).check_bibliography(Parameters(request)).await;

        assert!(
            text.contains("no DOI; resolve_missing_dois is false, so no lookup was made"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_429_on_one_lookup_marks_the_entry_upstream_with_its_retry_after() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/handles/{}", enc("10.5555/slow.1"))))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": 1})))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_json(json!({"message": "Daily limit reached"})),
            )
            .mount(&mock)
            .await;
        let bib = "@article{slow, title={Slow}, doi={10.5555/slow.1}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert!(
            text.contains("[slow] UPSTREAM doi:10.5555/slow.1"),
            "{text}"
        );
        assert!(text.contains("RATE LIMITED 10.5555/slow.1 : 429 Too Many Requests: Daily limit reached (Retry-After: 30)"), "{text}");
        assert!(!text.contains("] OK"), "{text}");
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        crate::server::promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(
            (
                structured["http_status"].as_u64(),
                structured["retry_after"].as_str()
            ),
            (Some(429), Some("30"))
        );
    }

    #[tokio::test]
    async fn a_402_on_one_lookup_marks_the_entry_upstream_with_its_retry_after() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/handles/{}", enc("10.5555/paid.1"))))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": 1})))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("retry-after", "86400")
                    .set_body_json(json!({"message": "Plan required"})),
            )
            .mount(&mock)
            .await;
        let bib = "@article{paid, title={Paid}, doi={10.5555/paid.1}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert!(
            text.contains("[paid] UPSTREAM doi:10.5555/paid.1"),
            "{text}"
        );
        let report = crate::http_error::upstream_failure_report(&text).unwrap();
        assert_eq!(
            (report.kind, report.http_status),
            ("payment_required", Some(402))
        );
        assert_eq!(report.retry_after.as_deref(), Some("86400"));
    }

    async fn mount_registered_and_in_corpus(mock: &MockServer, doi: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/handles/{}", enc(doi))))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": 1})))
            .mount(mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/lookup/doi"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"doi": doi, "title": "A mock record"})),
            )
            .mount(mock)
            .await;
    }

    async fn assert_provider_stop(
        text: &str,
        key: &str,
        marker: &str,
        kind: &str,
        status: u64,
        retry_after: &str,
    ) {
        assert!(text.contains(&format!("[{key}] UPSTREAM doi:")), "{text}");
        assert!(text.contains(marker), "{text}");
        assert!(!text.contains("UNCHECKED"), "{text}");
        assert!(!text.contains("] OK"), "{text}");
        let mut result = CallToolResult::success(vec![ContentBlock::text(text.to_string())]);
        crate::server::promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true), "{text}");
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["kind"], kind);
        assert_eq!(structured["http_status"].as_u64(), Some(status));
        assert_eq!(structured["retry_after"], retry_after);
    }

    #[tokio::test]
    async fn a_crossref_429_marks_the_entry_upstream_and_the_result_an_error() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_registered_and_in_corpus(&mock, "10.5555/crossref.429").await;
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc("10.5555/crossref.429"))))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "45")
                    .set_body_json(json!({"message": "Slow down"})),
            )
            .mount(&mock)
            .await;
        let bib = "@article{c429, title={Throttled at Crossref}, doi={10.5555/crossref.429}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert_provider_stop(&text, "c429", "RATE LIMITED", "rate_limited", 429, "45").await;
    }

    #[tokio::test]
    async fn a_crossref_402_marks_the_entry_upstream_and_the_result_an_error() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_registered_and_in_corpus(&mock, "10.5555/crossref.402").await;
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc("10.5555/crossref.402"))))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("retry-after", "600")
                    .set_body_json(json!({"message": "Plan required"})),
            )
            .mount(&mock)
            .await;
        let bib = "@article{c402, title={Walled at Crossref}, doi={10.5555/crossref.402}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert_provider_stop(
            &text,
            "c402",
            "PAYMENT REQUIRED",
            "payment_required",
            402,
            "600",
        )
        .await;
    }

    #[tokio::test]
    async fn a_datacite_402_marks_the_entry_upstream_and_the_result_an_error() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_registered_and_in_corpus(&mock, "10.5555/datacite.402").await;
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc("10.5555/datacite.402"))))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/dois/{}", enc("10.5555/datacite.402"))))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("retry-after", "900")
                    .set_body_json(json!({"errors": [{"title": "Plan required"}]})),
            )
            .mount(&mock)
            .await;
        let bib = "@misc{d402, title={Walled at DataCite}, doi={10.5555/datacite.402}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert_provider_stop(
            &text,
            "d402",
            "PAYMENT REQUIRED",
            "payment_required",
            402,
            "900",
        )
        .await;
    }

    #[tokio::test]
    async fn a_datacite_429_marks_the_entry_upstream_and_the_result_an_error() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_registered_and_in_corpus(&mock, "10.5555/datacite.429").await;
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc("10.5555/datacite.429"))))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/dois/{}", enc("10.5555/datacite.429"))))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "20"))
            .mount(&mock)
            .await;
        let bib = "@misc{d429, title={Throttled at DataCite}, doi={10.5555/datacite.429}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert_provider_stop(&text, "d429", "RATE LIMITED", "rate_limited", 429, "20").await;
    }

    #[tokio::test]
    async fn a_doi_org_refusal_is_unchecked_with_the_reason_never_broken() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        for status in [400u16, 401, 403, 409, 422] {
            let mock = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/handles/{}", enc("10.5555/refused.1"))))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(json!({"message": "nope"})),
                )
                .mount(&mock)
                .await;
            let bib = "@article{k, title={A paper that exists}, doi={10.5555/refused.1}}";

            let text = server(&mock)
                .check_bibliography(Parameters(args(bib)))
                .await;

            assert!(
                text.contains("[k] UNCHECKED doi:10.5555/refused.1"),
                "{status}: {text}"
            );
            assert!(text.contains(&status.to_string()), "{status}: {text}");
            assert!(
                !text.contains("BROKEN") && !text.contains("not registered"),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn a_transport_failure_is_an_error_result_that_keeps_the_doi_and_the_cause() {
        use crate::server::discovery::test_support::{closed_base, handle_ok, split};
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let slow = MockServer::start().await;
        handle_ok(&slow, "10.5555/t.4").await;
        Mock::given(method("GET"))
            .and(path(format!("/works/{}", enc("10.5555/t.4"))))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(3)))
            .mount(&slow)
            .await;
        let bib = "@article{k, title={A paper that exists}, doi={10.5555/t.4}}";
        for (elsewhere, cause) in [
            (slow.uri(), "timed out reading 127.0.0.1"),
            (closed_base(), "could not connect to 127.0.0.1"),
        ] {
            let text = split(&slow, &elsewhere)
                .check_bibliography(Parameters(args(bib)))
                .await;

            assert!(text.contains("[k] UPSTREAM doi:10.5555/t.4"), "{text}");
            assert!(
                text.contains(&format!("UNREACHABLE 10.5555/t.4 : {cause}")),
                "{text}"
            );
            assert!(!text.contains("UNCHECKED"), "{text}");
            let mut result = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
            crate::server::promote_upstream_failure(&mut result);
            assert_eq!(result.is_error, Some(true), "{text}");
            assert_eq!(result.structured_content.unwrap()["kind"], "unreachable");
        }
    }

    #[tokio::test]
    async fn a_doi_org_429_marks_the_entry_upstream_not_broken() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/handles/{}", enc("10.5555/handle.429"))))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "15"))
            .mount(&mock)
            .await;
        let bib = "@article{h429, title={Throttled at doi.org}, doi={10.5555/handle.429}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert!(!text.contains("BROKEN"), "{text}");
        assert_provider_stop(&text, "h429", "RATE LIMITED", "rate_limited", 429, "15").await;
    }

    #[tokio::test]
    async fn a_402_on_a_title_lookup_makes_the_result_an_error() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/resolve"))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("retry-after", "3600")
                    .set_body_json(json!({"message": "Plan required"})),
            )
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/reverse"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock)
            .await;
        let bib = "@article{t402, title={Fast crystallisation of sheared colloidal suspensions}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert!(text.contains("[t402] UPSTREAM"), "{text}");
        let mut result = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        crate::server::promote_upstream_failure(&mut result);
        assert_eq!(result.is_error, Some(true), "{text}");
        assert_eq!(
            result.structured_content.unwrap()["http_status"].as_u64(),
            Some(402)
        );
    }

    #[tokio::test]
    async fn one_title_lookup_makes_as_many_ookcite_calls_as_the_quota_check_counts() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        mount_bib_world(&mock).await;
        let bib = "@article{n, title={Fast crystallisation of sheared colloidal suspensions}, author={Smith, Jo}, year={2018}}";

        let text = server(&mock)
            .check_bibliography(Parameters(args(bib)))
            .await;

        assert!(text.contains("[n] OK doi:10.5555/found.2018"), "{text}");
        let ookcite_calls = mock
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().starts_with("/api/v1/"))
            .count();
        assert_eq!(ookcite_calls, TITLE_LOOKUP_COST);
    }

    #[tokio::test]
    async fn the_quota_check_counts_every_call_a_title_lookup_makes() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;
        let bib: String = (0..14)
            .map(|n| {
                format!("@article{{k{n}, title={{An entirely unknown manuscript number {n}}}}}\n")
            })
            .collect();

        let text = server(&mock)
            .check_bibliography(Parameters(args(&bib)))
            .await;

        assert!(
            text.starts_with("REFUSED: anonymous / no OOKCITE_API_KEY session"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_anonymous_paste_over_the_soft_cap_is_refused_before_any_request() {
        let _key = EnvGuard::unset("OOKCITE_API_KEY");
        let mock = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;
        let bib: String = (0..45)
            .map(|n| format!("@article{{k{n}, title={{Entry {n}}}, doi={{10.5555/many.{n}}}}}\n"))
            .collect();

        let text = server(&mock)
            .check_bibliography(Parameters(args(&bib)))
            .await;

        assert!(
            text.starts_with("REFUSED: anonymous / no OOKCITE_API_KEY session"),
            "{text}"
        );
    }

    #[test]
    fn a_malformed_doi_field_is_broken_and_not_searched() {
        let entry = parse_bibtex("@article{x, title={Some title here}, doi={nonsense}}")
            .unwrap()
            .remove(0);
        let row = row_for(&entry, &Found::Missing("x"), &HashMap::new());
        assert_eq!(row.status, Status::Broken);
        assert!(
            row.notes[0].contains("`nonsense` is not a DOI"),
            "{:?}",
            row.notes
        );
    }

    #[test]
    fn preprint_facts_read_crossref_and_datacite_and_the_arxiv_prefix() {
        let crossref = json!({"crossref": {
            "type": "posted-content",
            "relation": {"is-preprint-of": [{"id-type": "doi", "id": "https://doi.org/10.5555/PUB.1"}]}
        }});
        let facts = preprint_facts(&crossref, "10.1101/2020.01.01.1").unwrap();
        assert_eq!(facts.published, ["10.5555/pub.1"]);
        assert_eq!(facts.basis, "Crossref type posted-content");

        let datacite = json!({"datacite": {
            "types": {"resourceTypeGeneral": "Preprint"},
            "relatedIdentifiers": [
                {"relationType": "IsPreviousVersionOf", "relatedIdentifierType": "DOI", "relatedIdentifier": "10.5281/zenodo.2"},
                {"relationType": "IsPreprintOf", "relatedIdentifierType": "URL", "relatedIdentifier": "https://example.org"}
            ]
        }});
        let facts = preprint_facts(&datacite, "10.5281/zenodo.1").unwrap();
        assert!(
            facts.published.is_empty(),
            "only IsPreprintOf with a DOI counts: {facts:?}"
        );
        assert_eq!(facts.basis, "DataCite resource type Preprint");

        let by_prefix = preprint_facts(&json!({}), "10.48550/arxiv.2201.00001").unwrap();
        assert_eq!(by_prefix.basis, "arXiv DOI prefix");
        assert!(
            preprint_facts(&json!({"crossref": {"type": "journal-article"}}), "10.1/x").is_none()
        );
    }

    #[test]
    fn duplicate_titles_need_agreeing_years_and_authors_and_enough_letters() {
        let entries = parse_bibtex(
            "@article{a, title={Crystal nucleation in sheared colloidal glasses}, year={2020}, author={Ada Example}}
             @article{b, title={Crystal Nucleation in Sheared Colloidal Glasses.}, year={2020}, author={Example, Ada}}
             @article{c, title={Crystal nucleation in sheared colloidal glasses}, year={2015}}
             @article{d, title={Crystal nucleation in sheared colloidal glasses}, year={2021}}
             @article{e, title={Short}, year={2020}}
             @article{f, title={Short}, year={2020}}",
        )
        .unwrap();
        let none = vec![None; entries.len()];
        let groups = duplicate_groups(&entries, &none, &HashMap::new());
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].0, vec![0, 1]);
        assert_eq!(groups[0].1, "same title");
    }

    #[test]
    fn a_title_match_is_refused_on_a_year_or_first_author_conflict() {
        let entry = parse_bibtex(
            "@article{x, title={Fast crystallisation of sheared colloidal suspensions}, author={Smith, Jo}, year={2018}}",
        )
        .unwrap()
        .remove(0);
        let answer = |year: i64, family: &str| {
            json!({
                "doi": "10.5555/f.1",
                "title": "Fast crystallisation of sheared colloidal suspensions",
                "date": {"year": year},
                "authors": [{"family": family}]
            })
        };
        assert!(matches!(
            accept_title_match(&entry, &answer(2018, "Smith")),
            Found::FromTitle { .. }
        ));
        assert!(
            matches!(accept_title_match(&entry, &answer(2017, "Smith")), Found::Rejected(why) if why.contains("is from 2017"))
        );
        assert!(
            matches!(accept_title_match(&entry, &answer(2018, "Jones")), Found::Rejected(why) if why.contains("does not list smith"))
        );
        assert!(matches!(
            accept_title_match(&entry, &json!({"title": "x"})),
            Found::Missing(_)
        ));
    }

    #[test]
    fn the_input_schema_is_closed_bounded_and_names_every_format() {
        let tool = Server::new_with_base("http://127.0.0.1:9".into())
            .get_tool("check_bibliography")
            .expect("tool is registered");
        let schema = Value::Object((*tool.input_schema).clone());
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["content"]));
        let content = &schema["properties"]["content"];
        assert_eq!(
            (content["minLength"].as_i64(), content["maxLength"].as_i64()),
            (Some(5), Some(1_048_576))
        );
        assert_eq!(
            schema["properties"]["resolve_missing_dois"]["default"],
            true
        );
        let text = schema.to_string();
        for format in ["auto", "bibtex", "ris"] {
            assert!(text.contains(&format!("\"{format}\"")), "{text}");
        }
        assert!(serde_json::from_value::<CheckBibliographyArgs>(
            json!({"content": "@a{b}", "style": "x"})
        )
        .is_err());
    }

    #[test]
    fn the_readme_transcript_is_the_tool_output() {
        let readme = include_str!("../../../README.md");
        let entries = parse_bibtex(BIB).unwrap();
        let founds: Vec<Found> = entries
            .iter()
            .map(|entry| match (&entry.doi, entry.key.as_str()) {
                (Some(doi), _) => Found::Given(doi.clone()),
                (None, "nodoi2018") => Found::FromTitle {
                    doi: "10.5555/found.2018".into(),
                    overlap: 1.0,
                },
                _ => Found::Missing("no record matches the title"),
            })
            .collect();
        let read = |notices: Vec<PublisherNotice>, preprint: Option<PreprintFacts>| {
            DoiState::Read(DoiFacts {
                notices,
                preprint,
                unread: None,
            })
        };
        let mut states = HashMap::new();
        for doi in [
            "10.5555/clean.2020",
            "10.5555/pub.2021",
            "10.5555/found.2018",
        ] {
            states.insert(doi.to_string(), read(vec![], None));
        }
        states.insert(
            "10.5555/retracted.2019".into(),
            read(
                vec![PublisherNotice {
                    kind: NoticeKind::Retraction,
                    doi: Some("10.5555/notice.2020".into()),
                    date: Some("2020-06".into()),
                    from_title: false,
                }],
                None,
            ),
        );
        states.insert(
            "10.5555/preprint.2021".into(),
            read(
                vec![],
                Some(PreprintFacts {
                    published: vec!["10.5555/pub.2021".into()],
                    basis: "Crossref type posted-content",
                }),
            ),
        );
        states.insert(
            "10.48550/arxiv.2201.00001".into(),
            read(
                vec![],
                Some(PreprintFacts {
                    published: vec!["10.5555/pub.2022".into()],
                    basis: "DataCite resource type Preprint",
                }),
            ),
        );
        let output = render_check("BibTeX", &entries, &founds, &states);
        assert!(
            readme.contains(&output),
            "README transcript for check_bibliography is stale; it should contain:\n{output}"
        );
    }
}
