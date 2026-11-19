//! Parse a manuscript `.bib` and report broken or retracted entries.
//!
//! The HTTP calls stay on `Server`. This module owns the BibTeX parse, the
//! publisher-notice reading, and the text the tool returns. Counts in that
//! text are the number of parsed entries in each status, not an estimate.

use std::collections::HashMap;

use serde_json::Value;

use crate::bibliographic::{claim_verdict, ClaimStatus};
use crate::collection_entries::{looks_like_doi_token, normalize_doi_token};
use crate::tool_args::BibliographicClaim;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BibEntry {
    pub key: String,
    pub entry_type: String,
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub year: Option<i32>,
    pub journal: Option<String>,
    pub volume: Option<String>,
    pub issue: Option<String>,
    pub pages: Option<String>,
    /// Normalized DOI from the `doi` field or a doi.org `url`.
    pub doi: Option<String>,
    /// The `doi` field was present and was not a DOI.
    pub malformed_doi: Option<String>,
}

/// One bibliography entry with field values as written, outer delimiters removed.
/// Brace groups, `--`, and LaTeX commands inside a value are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBibEntry {
    pub key: String,
    pub entry_type: String,
    pub fields: Vec<(String, String)>,
}

impl BibEntry {
    fn claim(&self) -> BibliographicClaim {
        BibliographicClaim {
            title: self.title.clone(),
            authors: if self.authors.is_empty() {
                None
            } else {
                Some(self.authors.clone())
            },
            year: self.year,
            journal: self.journal.clone(),
            volume: self.volume.clone(),
            issue: self.issue.clone(),
            pages: self.pages.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Correction,
    Concern,
    Withdrawal,
    Retraction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherNotice {
    pub kind: NoticeKind,
    pub doi: Option<String>,
    pub date: Option<String>,
    pub from_title: bool,
}

impl PublisherNotice {
    pub(crate) fn line(&self) -> String {
        if self.from_title {
            return match self.kind {
                NoticeKind::Retraction => "title marked retracted".into(),
                NoticeKind::Withdrawal => "title marked withdrawn".into(),
                NoticeKind::Concern => "title marked as an expression of concern".into(),
                NoticeKind::Correction => "title marked as a correction".into(),
            };
        }
        let verb = match self.kind {
            NoticeKind::Retraction => "retracted by",
            NoticeKind::Withdrawal => "withdrawn by",
            NoticeKind::Concern => "expression of concern",
            NoticeKind::Correction => "correction",
        };
        match (self.doi.as_deref(), self.date.as_deref()) {
            (None, None) => format!("{verb} recorded by the publisher"),
            (Some(doi), None) => format!("{verb} doi:{doi}"),
            (None, Some(date)) => format!("{verb} on {date}"),
            (Some(doi), Some(date)) => format!("{verb} doi:{doi} on {date}"),
        }
    }
}

/// Why an entry is BROKEN when doi.org answers `responseCode` 100.
pub const BROKEN_NOT_REGISTERED: &str = "not registered at doi.org";

/// Why an entry is BROKEN when the DOI is registered but the OokCite corpus,
/// Crossref and DataCite all lack a record of it.
pub const BROKEN_NO_RECORD: &str = "not in the OokCite corpus and no Crossref or DataCite record";

/// What one DOI lookup produced. The server fills this; the report does not
/// call the network.
#[derive(Debug, Clone)]
pub enum DoiOutcome {
    Broken(String),
    Upstream(String),
    Resolved {
        metadata: Value,
        provider: Option<Value>,
        provider_error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditStatus {
    Ok,
    Retracted,
    Withdrawn,
    Concern,
    Broken,
    Mismatch,
    Incomplete,
    Unchecked,
    NoDoi,
    Upstream,
}

impl AuditStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Retracted => "RETRACTED",
            Self::Withdrawn => "WITHDRAWN",
            Self::Concern => "CONCERN",
            Self::Broken => "BROKEN",
            Self::Mismatch => "MISMATCH",
            Self::Incomplete => "INCOMPLETE",
            Self::Unchecked => "UNCHECKED",
            Self::NoDoi => "NO DOI",
            Self::Upstream => "UPSTREAM",
        }
    }
}

/// Stable order for the summary line. A zero is left out; the entry total
/// still counts it.
const SUMMARY_ORDER: [AuditStatus; 10] = [
    AuditStatus::Ok,
    AuditStatus::Retracted,
    AuditStatus::Withdrawn,
    AuditStatus::Concern,
    AuditStatus::Broken,
    AuditStatus::Mismatch,
    AuditStatus::Incomplete,
    AuditStatus::Unchecked,
    AuditStatus::Upstream,
    AuditStatus::NoDoi,
];

struct Row {
    status: AuditStatus,
    head: String,
    details: Vec<String>,
}

/// Numbers the refusal message is allowed to use. They come from the
/// catalogue constants the caller already measured.
pub struct AuditQuota {
    pub metered: u32,
    pub has_api_key: bool,
    pub remaining: Option<u32>,
    pub limit: Option<u32>,
    pub plan: String,
    pub anon_soft_cap: u32,
    pub anon_daily: u32,
    pub free_daily: u32,
}

pub fn audit_quota_refusal(quota: &AuditQuota) -> Option<String> {
    if quota.metered == 0 {
        return None;
    }
    if let (true, Some(remaining)) = (quota.has_api_key, quota.remaining) {
        if quota.metered > remaining {
            let plan = if quota.plan.is_empty() {
                "?"
            } else {
                quota.plan.as_str()
            };
            let limit_hint = quota.limit.map_or_else(
                || format!(" (plan {plan})"),
                |limit| format!(" (plan {plan} limit {limit}/day)"),
            );
            return Some(format!(
                "REFUSED: audit needs {} lookup(s) but only {remaining} remaining{limit_hint}. \
                 Shrink the bibliography, upgrade the plan, or wait for the daily reset. \
                 A collection membership does not skip this audit.",
                quota.metered
            ));
        }
        return None;
    }
    if quota.metered > quota.anon_soft_cap {
        if quota.has_api_key {
            return Some(format!(
                "REFUSED: lookup quota was not confirmed, refusing an audit of {} DOIs \
                 (soft cap {}). A collection membership does not skip this audit.",
                quota.metered, quota.anon_soft_cap
            ));
        }
        return Some(format!(
            "REFUSED: anonymous / no OOKCITE_API_KEY session, refusing an audit of {} DOIs \
             (soft cap {}). Free accounts raise the limit to {}/day: \
             sign up at https://my.turtletech.us/signup?service=ookcite&source=ookcite_mcp, then set \
             OOKCITE_API_KEY. Or shrink the bibliography. IP daily limits still apply \
             (~{}/day anonymous). A collection membership does not skip this audit.",
            quota.metered, quota.anon_soft_cap, quota.free_daily, quota.anon_daily
        ));
    }
    None
}

pub fn parse_bibtex(content: &str) -> Result<Vec<BibEntry>, String> {
    if content.len() > crate::constants::AUDIT_BIB_MAX_BYTES {
        return Err(format!(
            "bibliography is {} bytes; the limit is {}",
            content.len(),
            crate::constants::AUDIT_BIB_MAX_BYTES
        ));
    }
    let mut scan = Scan { src: content, i: 0 };
    let mut entries = Vec::new();
    while scan.i < scan.src.len() {
        scan.skip_trivia();
        if scan.i >= scan.src.len() {
            break;
        }
        if scan.peek() != Some('@') {
            scan.bump();
            continue;
        }
        let start = scan.i;
        scan.bump();
        let entry_type = scan.read_ident();
        if entry_type.is_empty() {
            continue;
        }
        if is_directive(&entry_type) {
            scan.skip_directive()?;
            continue;
        }
        scan.skip_trivia();
        let closer = match scan.bump() {
            Some('{') => '}',
            Some('(') => ')',
            _ => {
                return Err(format!(
                    "entry @{entry_type} at byte {start} is missing its opening brace"
                ));
            }
        };
        let key = scan.read_key(closer)?;
        let fields = scan.read_fields(closer)?;
        entries.push(entry_from(entry_type, key, fields));
        if entries.len() > crate::constants::AUDIT_BIB_MAX_ENTRIES {
            return Err(format!(
                "bibliography has more than {} entries",
                crate::constants::AUDIT_BIB_MAX_ENTRIES
            ));
        }
    }
    Ok(entries)
}

/// Entries in file order. Field order is the source order. Values keep braces.
pub fn parse_bibtex_raw(content: &str) -> Result<Vec<RawBibEntry>, String> {
    if content.len() > crate::constants::AUDIT_BIB_MAX_BYTES {
        return Err(format!(
            "bibliography is {} bytes; the limit is {}",
            content.len(),
            crate::constants::AUDIT_BIB_MAX_BYTES
        ));
    }
    let mut scan = Scan { src: content, i: 0 };
    let mut entries = Vec::new();
    while scan.i < scan.src.len() {
        scan.skip_trivia();
        if scan.i >= scan.src.len() {
            break;
        }
        if scan.peek() != Some('@') {
            scan.bump();
            continue;
        }
        let start = scan.i;
        scan.bump();
        let entry_type = scan.read_ident();
        if entry_type.is_empty() {
            continue;
        }
        if is_directive(&entry_type) {
            scan.skip_directive()?;
            continue;
        }
        scan.skip_trivia();
        let closer = match scan.bump() {
            Some('{') => '}',
            Some('(') => ')',
            _ => {
                return Err(format!(
                    "entry @{entry_type} at byte {start} is missing its opening brace"
                ));
            }
        };
        let key = scan.read_key(closer)?;
        let fields = scan.read_fields_ordered(closer)?;
        entries.push(RawBibEntry {
            key,
            entry_type: entry_type.to_ascii_lowercase(),
            fields,
        });
        if entries.len() > crate::constants::AUDIT_BIB_MAX_ENTRIES {
            return Err(format!(
                "bibliography has more than {} entries",
                crate::constants::AUDIT_BIB_MAX_ENTRIES
            ));
        }
    }
    Ok(entries)
}

fn is_directive(entry_type: &str) -> bool {
    matches!(
        entry_type.to_ascii_lowercase().as_str(),
        "comment" | "string" | "preamble"
    )
}

fn entry_from(entry_type: String, key: String, fields: HashMap<String, String>) -> BibEntry {
    let entry_type = entry_type.to_ascii_lowercase();
    let (doi, malformed_doi) = doi_from_fields(&fields);
    BibEntry {
        key,
        title: fields.get("title").map(|value| plain_text(value)),
        authors: fields
            .get("author")
            .map(|value| split_authors(value))
            .unwrap_or_default(),
        year: fields
            .get("year")
            .and_then(|value| first_year(value))
            .or_else(|| fields.get("date").and_then(|value| first_year(value))),
        journal: fields.get("journal").map(|value| plain_text(value)),
        volume: fields.get("volume").map(|value| plain_text(value)),
        issue: issue_field(&entry_type, &fields),
        pages: fields.get("pages").map(|value| plain_text(value)),
        doi,
        malformed_doi,
        entry_type,
    }
}

fn issue_field(entry_type: &str, fields: &HashMap<String, String>) -> Option<String> {
    if let Some(issue) = fields.get("issue").map(|value| plain_text(value)) {
        return Some(issue);
    }
    if matches!(
        entry_type,
        "article" | "inproceedings" | "incollection" | "inbook"
    ) {
        return fields.get("number").map(|value| plain_text(value));
    }
    None
}

fn doi_from_fields(fields: &HashMap<String, String>) -> (Option<String>, Option<String>) {
    if let Some(raw) = fields.get("doi") {
        let shown = plain_text(raw);
        return match clean_doi(&shown) {
            Some(doi) => (Some(doi), None),
            None => (None, Some(shown)),
        };
    }
    if let Some(raw) = fields.get("url") {
        if let Some(doi) = clean_doi(&plain_text(raw)) {
            return (Some(doi), None);
        }
    }
    (None, None)
}

pub(crate) fn clean_doi(raw: &str) -> Option<String> {
    let glued: String = raw.split_whitespace().collect();
    let trimmed = glued.trim().trim_end_matches(['.', ',', ';', ')']);
    let lower = trimmed.to_ascii_lowercase();
    let rest = strip_doi_prefix(&lower);
    if looks_like_doi_token(rest) {
        Some(normalize_doi_token(rest))
    } else {
        None
    }
}

fn strip_doi_prefix(lower: &str) -> &str {
    const PREFIXES: [&str; 7] = [
        "https://doi.org/doi/",
        "http://doi.org/doi/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "https://doi.org/",
        "http://doi.org/",
        "doi:",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return rest;
        }
    }
    lower
}

fn plain_text(value: &str) -> String {
    let stripped: String = value
        .chars()
        .filter(|ch| *ch != '{' && *ch != '}')
        .collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_year(value: &str) -> Option<i32> {
    let digits: String = value
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.len() != 4 {
        return None;
    }
    let year = digits.parse::<i32>().ok()?;
    if (1000..3000).contains(&year) {
        Some(year)
    } else {
        None
    }
}

fn split_authors(value: &str) -> Vec<String> {
    let flat = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    let mut authors = Vec::new();
    let mut current = String::new();
    let mut depth: usize = 0;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '{' {
            depth += 1;
            current.push(ch);
            index += 1;
            continue;
        }
        if ch == '}' {
            depth = depth.saturating_sub(1);
            current.push(ch);
            index += 1;
            continue;
        }
        if depth == 0 && chars[index..].starts_with(&[' ', 'a', 'n', 'd', ' ']) {
            push_author(&mut authors, &current);
            current.clear();
            index += 5;
            continue;
        }
        current.push(ch);
        index += 1;
    }
    push_author(&mut authors, &current);
    authors
}

fn push_author(authors: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let unwrapped = unwrap_outer_braces(trimmed);
    let plain = plain_text(unwrapped);
    if !plain.is_empty() {
        authors.push(plain);
    }
}

fn unwrap_outer_braces(value: &str) -> &str {
    let mut current = value.trim();
    while current.starts_with('{') && current.ends_with('}') && current.len() >= 2 {
        let inner = &current[1..current.len() - 1];
        if brace_depth_returns_to_zero(inner) {
            break;
        }
        current = inner.trim();
    }
    current
}

fn brace_depth_returns_to_zero(value: &str) -> bool {
    let mut depth: usize = 0;
    for ch in value.chars() {
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            if depth == 0 {
                return true;
            }
            depth -= 1;
        }
    }
    false
}

pub fn notices_from_provider(provider: Option<&Value>) -> Vec<PublisherNotice> {
    let Some(provider) = provider else {
        return Vec::new();
    };
    let mut notices = Vec::new();
    if let Some(message) = provider.get("crossref").filter(|value| value.is_object()) {
        collect_crossref(message, &mut notices);
    } else if provider.get("update-to").is_some() || provider.get("relation").is_some() {
        collect_crossref(provider, &mut notices);
    }
    if let Some(attrs) = provider.get("datacite").filter(|value| value.is_object()) {
        collect_datacite(attrs, &mut notices);
    } else if provider.get("relatedIdentifiers").is_some() {
        collect_datacite(provider, &mut notices);
    }
    dedup_notices(notices)
}

fn collect_crossref(message: &Value, notices: &mut Vec<PublisherNotice>) {
    if let Some(updates) = message.get("update-to").and_then(Value::as_array) {
        for update in updates {
            let Some(kind) = update
                .get("type")
                .and_then(Value::as_str)
                .and_then(notice_kind)
            else {
                continue;
            };
            notices.push(PublisherNotice {
                kind,
                doi: update
                    .get("DOI")
                    .or_else(|| update.get("doi"))
                    .and_then(Value::as_str)
                    .and_then(clean_doi),
                date: update.get("updated").and_then(date_from),
                from_title: false,
            });
        }
    }
    if let Some(relation) = message.get("relation").and_then(Value::as_object) {
        for (key, value) in relation {
            let Some(kind) = relation_kind(key) else {
                continue;
            };
            let Some(items) = value.as_array() else {
                continue;
            };
            for item in items {
                notices.push(PublisherNotice {
                    kind,
                    doi: item
                        .get("id")
                        .or_else(|| item.get("DOI"))
                        .or_else(|| item.get("doi"))
                        .and_then(Value::as_str)
                        .and_then(clean_doi),
                    date: None,
                    from_title: false,
                });
            }
        }
    }
    if let Some(title) = message
        .get("title")
        .and_then(Value::as_array)
        .and_then(|titles| titles.first())
        .and_then(Value::as_str)
        .and_then(title_mark)
    {
        notices.push(title_notice(title));
    }
}

fn collect_datacite(attrs: &Value, notices: &mut Vec<PublisherNotice>) {
    if let Some(relations) = attrs.get("relatedIdentifiers").and_then(Value::as_array) {
        for relation in relations {
            let Some(kind) = relation
                .get("relationType")
                .and_then(Value::as_str)
                .and_then(datacite_kind)
            else {
                continue;
            };
            notices.push(PublisherNotice {
                kind,
                doi: relation
                    .get("relatedIdentifier")
                    .and_then(Value::as_str)
                    .and_then(clean_doi),
                date: None,
                from_title: false,
            });
        }
    }
    if let Some(title) = attrs
        .get("titles")
        .and_then(Value::as_array)
        .and_then(|titles| titles.first())
        .and_then(|title| title.get("title"))
        .and_then(Value::as_str)
        .and_then(title_mark)
    {
        notices.push(title_notice(title));
    }
}

fn title_notice(kind: NoticeKind) -> PublisherNotice {
    PublisherNotice {
        kind,
        doi: None,
        date: None,
        from_title: true,
    }
}

fn dedup_notices(notices: Vec<PublisherNotice>) -> Vec<PublisherNotice> {
    let mut out = Vec::new();
    for notice in notices {
        if let Some(existing) = out
            .iter_mut()
            .find(|existing| same_notice(existing, &notice))
        {
            if existing.date.is_none() {
                existing.date = notice.date;
            }
            continue;
        }
        out.push(notice);
    }
    out
}

fn same_notice(left: &PublisherNotice, right: &PublisherNotice) -> bool {
    if left.kind != right.kind || left.from_title != right.from_title {
        return false;
    }
    match (&left.doi, &right.doi) {
        (Some(left_doi), Some(right_doi)) => left_doi == right_doi,
        (None, None) => left.date == right.date,
        _ => false,
    }
}

fn notice_kind(raw: &str) -> Option<NoticeKind> {
    match alnum_key(raw).as_str() {
        "retraction" => Some(NoticeKind::Retraction),
        "withdrawal" => Some(NoticeKind::Withdrawal),
        "expressionofconcern" => Some(NoticeKind::Concern),
        "correction" | "erratum" => Some(NoticeKind::Correction),
        _ => None,
    }
}

fn relation_kind(raw: &str) -> Option<NoticeKind> {
    match alnum_key(raw).as_str() {
        "isretractedby" => Some(NoticeKind::Retraction),
        "iswithdrawnby" => Some(NoticeKind::Withdrawal),
        "isexpressionofconcern" | "hasexpressionofconcern" => Some(NoticeKind::Concern),
        "iscorrectedby" => Some(NoticeKind::Correction),
        _ => None,
    }
}

fn datacite_kind(raw: &str) -> Option<NoticeKind> {
    match alnum_key(raw).as_str() {
        "isretractedby" => Some(NoticeKind::Retraction),
        "iswithdrawnby" => Some(NoticeKind::Withdrawal),
        _ => None,
    }
}

fn alnum_key(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn title_mark(title: &str) -> Option<NoticeKind> {
    let lower = title.trim().to_ascii_lowercase();
    if lower.starts_with("retracted:") || lower.starts_with("retracted :") {
        Some(NoticeKind::Retraction)
    } else if lower.starts_with("withdrawn:") || lower.starts_with("withdrawn :") {
        Some(NoticeKind::Withdrawal)
    } else {
        None
    }
}

fn date_from(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let parts = value.get("date-parts")?.as_array()?.first()?.as_array()?;
    let year = parts.first()?.as_i64()?;
    if !(1000..3000).contains(&year) {
        return None;
    }
    let mut out = format!("{year:04}");
    if let Some(month) = parts.get(1).and_then(Value::as_i64) {
        if (1..=12).contains(&month) {
            out.push_str(&format!("-{month:02}"));
            if let Some(day) = parts.get(2).and_then(Value::as_i64) {
                if (1..=31).contains(&day) {
                    out.push_str(&format!("-{day:02}"));
                }
            }
        }
    }
    Some(out)
}

pub fn render_audit(entries: &[BibEntry], outcomes: &HashMap<String, DoiOutcome>) -> String {
    let rows: Vec<Row> = entries
        .iter()
        .map(|entry| row_for(entry, outcomes))
        .collect();
    let with_doi = entries.iter().filter(|entry| entry.doi.is_some()).count();
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(summary(&rows, with_doi));
    out.push(String::new());
    for (index, row) in rows.iter().enumerate() {
        out.push(format!("{}. {}", index + 1, row.head));
        out.extend(row.details.iter().cloned());
    }
    out.join("\n")
}

fn summary(rows: &[Row], with_doi: usize) -> String {
    let entries = rows.len();
    let mut parts = Vec::new();
    for status in SUMMARY_ORDER {
        let count = rows.iter().filter(|row| row.status == status).count();
        if count > 0 {
            parts.push(format!("{count} {}", status.label()));
        }
    }
    let breakdown = if parts.is_empty() {
        "0 entries".to_string()
    } else {
        format!("{}.", parts.join(", "))
    };
    format!("Audited {entries} entries, {with_doi} with a DOI.\n{breakdown}")
}

fn row_for(entry: &BibEntry, outcomes: &HashMap<String, DoiOutcome>) -> Row {
    let key = if entry.key.is_empty() {
        "(unnamed)"
    } else {
        entry.key.as_str()
    };
    if let Some(raw) = &entry.malformed_doi {
        return Row {
            status: AuditStatus::Broken,
            head: format!("{key} | BROKEN | doi field is not a DOI: {raw}"),
            details: Vec::new(),
        };
    }
    let Some(doi) = &entry.doi else {
        let title = entry
            .title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or("(no title)");
        return Row {
            status: AuditStatus::NoDoi,
            head: format!("{key} | NO DOI | {title}"),
            details: vec!["no DOI in the entry; this audit does not guess one".to_string()],
        };
    };
    let Some(outcome) = outcomes.get(doi) else {
        return Row {
            status: AuditStatus::Upstream,
            head: format!("{key} | UPSTREAM | doi:{doi}"),
            details: vec!["lookup did not return a result for this DOI".into()],
        };
    };
    match outcome {
        DoiOutcome::Broken(reason) => Row {
            status: AuditStatus::Broken,
            head: format!("{key} | BROKEN | doi:{doi} | {reason}"),
            details: Vec::new(),
        },
        DoiOutcome::Upstream(detail) => {
            let title = entry.title.as_deref().unwrap_or("(no title)");
            Row {
                status: AuditStatus::Upstream,
                head: format!("{key} | UPSTREAM | doi:{doi} | {title}"),
                details: vec![detail.clone()],
            }
        }
        DoiOutcome::Resolved {
            metadata,
            provider,
            provider_error,
        } => resolved_row(
            entry,
            key,
            doi,
            metadata,
            provider.as_ref(),
            provider_error.as_deref(),
        ),
    }
}

fn resolved_row(
    entry: &BibEntry,
    key: &str,
    doi: &str,
    metadata: &Value,
    provider: Option<&Value>,
    provider_error: Option<&str>,
) -> Row {
    let notices = notices_from_provider(provider);
    let verdict = claim_verdict(metadata, &entry.claim());
    let retraction_checked = provider.is_some() && provider_error.is_none();
    let (status, mut details) = classify(&notices, &verdict, retraction_checked);
    if let Some(note) = manuscript_title_note(entry.title.as_deref(), &notices) {
        details.push(note);
    }
    if let Some(error) = provider_error {
        details.push(format!("retraction unchecked: {error}"));
    } else if provider.is_none() {
        details.push("retraction unchecked: no Crossref or DataCite record was read".to_string());
    }
    let title = metadata
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .or(entry.title.as_deref().filter(|title| !title.is_empty()))
        .unwrap_or("(no title)");
    Row {
        status,
        head: format!("{key} | {} | doi:{doi} | {title}", status.label()),
        details,
    }
}

fn manuscript_title_note(title: Option<&str>, notices: &[PublisherNotice]) -> Option<String> {
    let kind = title.and_then(title_mark)?;
    if notices.iter().any(|notice| notice.kind == kind) {
        return None;
    }
    Some(match kind {
        NoticeKind::Retraction => {
            "manuscript title is marked retracted; the publisher record does not say so".into()
        }
        NoticeKind::Withdrawal => {
            "manuscript title is marked withdrawn; the publisher record does not say so".into()
        }
        NoticeKind::Concern | NoticeKind::Correction => return None,
    })
}

fn classify(
    notices: &[PublisherNotice],
    verdict: &crate::bibliographic::ClaimVerdict,
    retraction_checked: bool,
) -> (AuditStatus, Vec<String>) {
    let mut details: Vec<String> = notices.iter().map(PublisherNotice::line).collect();
    details.extend(verdict.lines.iter().cloned());
    let severe = notices
        .iter()
        .filter(|notice| notice.kind != NoticeKind::Correction)
        .map(|notice| notice.kind)
        .max_by_key(|kind| kind_rank(*kind));
    if let Some(kind) = severe {
        let status = match kind {
            NoticeKind::Retraction => AuditStatus::Retracted,
            NoticeKind::Withdrawal => AuditStatus::Withdrawn,
            NoticeKind::Concern => AuditStatus::Concern,
            NoticeKind::Correction => AuditStatus::Ok,
        };
        return (status, details);
    }
    if !retraction_checked {
        let status = match verdict.status {
            ClaimStatus::Mismatch => AuditStatus::Mismatch,
            ClaimStatus::Incomplete => AuditStatus::Incomplete,
            ClaimStatus::Match => AuditStatus::Unchecked,
        };
        return (status, details);
    }
    let status = match verdict.status {
        ClaimStatus::Match => AuditStatus::Ok,
        ClaimStatus::Mismatch => AuditStatus::Mismatch,
        ClaimStatus::Incomplete => AuditStatus::Incomplete,
    };
    (status, details)
}

fn kind_rank(kind: NoticeKind) -> u8 {
    match kind {
        NoticeKind::Correction => 0,
        NoticeKind::Concern => 1,
        NoticeKind::Withdrawal => 2,
        NoticeKind::Retraction => 3,
    }
}

struct Scan<'a> {
    src: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    fn peek(&self) -> Option<char> {
        self.src[self.i..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.i += ch.len_utf8();
        Some(ch)
    }

    fn skip_trivia(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.peek() == Some('%') {
                while let Some(ch) = self.bump() {
                    if ch == '\n' {
                        break;
                    }
                }
                continue;
            }
            break;
        }
    }

    fn read_ident(&mut self) -> String {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == ':' {
                out.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        out
    }

    fn skip_directive(&mut self) -> Result<(), String> {
        self.skip_trivia();
        match self.peek() {
            Some('{') => {
                self.bump();
                self.skip_group('}')
            }
            Some('(') => {
                self.bump();
                self.skip_group(')')
            }
            _ => {
                while let Some(ch) = self.bump() {
                    if ch == '\n' {
                        break;
                    }
                }
                Ok(())
            }
        }
    }

    fn skip_group(&mut self, closer: char) -> Result<(), String> {
        let opener = if closer == '}' { '{' } else { '(' };
        let mut depth: usize = 1;
        while let Some(ch) = self.bump() {
            if ch == opener {
                depth += 1;
            } else if ch == closer {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
        }
        Err("unclosed BibTeX directive".into())
    }

    fn read_key(&mut self, closer: char) -> Result<String, String> {
        let mut key = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err("BibTeX entry ended before its citation key".into());
            };
            if ch == ',' || ch == closer {
                break;
            }
            key.push(ch);
            self.bump();
        }
        Ok(key.trim().to_string())
    }

    fn read_fields(&mut self, closer: char) -> Result<HashMap<String, String>, String> {
        let ordered = self.read_fields_ordered(closer)?;
        let mut fields = HashMap::new();
        for (name, value) in ordered {
            fields.insert(name, value);
        }
        Ok(fields)
    }

    fn read_fields_ordered(&mut self, closer: char) -> Result<Vec<(String, String)>, String> {
        self.skip_trivia();
        if self.peek() == Some(',') {
            self.bump();
        }
        let mut fields = Vec::new();
        loop {
            self.skip_trivia();
            let Some(ch) = self.peek() else {
                return Err("BibTeX entry ended before it closed".into());
            };
            if ch == closer {
                self.bump();
                break;
            }
            if ch == ',' {
                self.bump();
                continue;
            }
            let name_at = self.i;
            let name = self.read_ident();
            if name.is_empty() {
                return Err(format!("BibTeX field name missing at byte {name_at}"));
            }
            self.skip_trivia();
            if self.bump() != Some('=') {
                return Err(format!("BibTeX field {name} is missing '='"));
            }
            self.skip_trivia();
            let value = self.read_value()?;
            if value.len() > crate::constants::AUDIT_BIB_MAX_FIELD_BYTES {
                return Err(format!(
                    "BibTeX field {name} is {} bytes; the limit is {}",
                    value.len(),
                    crate::constants::AUDIT_BIB_MAX_FIELD_BYTES
                ));
            }
            let name = name.to_ascii_lowercase();
            if let Some(existing) = fields.iter_mut().find(|(field, _)| field == &name) {
                existing.1 = value;
            } else {
                fields.push((name, value));
            }
            self.skip_trivia();
            if self.peek() == Some(',') {
                self.bump();
            }
        }
        Ok(fields)
    }

    fn read_value(&mut self) -> Result<String, String> {
        let mut parts = Vec::new();
        loop {
            self.skip_trivia();
            let part = match self.peek() {
                Some('{') => self.read_braced()?,
                Some('"') => self.read_quoted()?,
                Some(ch) if ch.is_ascii_digit() => self.read_number(),
                Some(_) => self.read_macro()?,
                None => return Err("BibTeX value ended early".into()),
            };
            parts.push(part);
            self.skip_trivia();
            if self.peek() == Some('#') {
                self.bump();
                continue;
            }
            break;
        }
        Ok(parts.concat())
    }

    fn read_braced(&mut self) -> Result<String, String> {
        self.bump();
        let mut out = String::new();
        let mut depth: usize = 1;
        while let Some(ch) = self.bump() {
            if ch == '{' {
                depth += 1;
                out.push(ch);
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
                out.push(ch);
            } else {
                out.push(ch);
            }
        }
        Err("unclosed brace in a BibTeX value".into())
    }

    fn read_quoted(&mut self) -> Result<String, String> {
        self.bump();
        let mut out = String::new();
        let mut depth: usize = 0;
        while let Some(ch) = self.bump() {
            if ch == '{' {
                depth += 1;
                out.push(ch);
            } else if ch == '}' {
                depth = depth.saturating_sub(1);
                out.push(ch);
            } else if ch == '"' && depth == 0 {
                return Ok(out);
            } else {
                out.push(ch);
            }
        }
        Err("unclosed quote in a BibTeX value".into())
    }

    fn read_number(&mut self) -> String {
        let mut out = String::new();
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            out.push(self.bump().unwrap_or_default());
        }
        out
    }

    fn read_macro(&mut self) -> Result<String, String> {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() || matches!(ch, '#' | ',' | '}' | ')' | '=') {
                break;
            }
            out.push(ch);
            self.bump();
        }
        if out.is_empty() {
            Err("empty BibTeX value".into())
        } else {
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn quota(metered: u32, has_api_key: bool, remaining: Option<u32>) -> AuditQuota {
        AuditQuota {
            metered,
            has_api_key,
            remaining,
            limit: Some(60),
            plan: "free".into(),
            anon_soft_cap: crate::constants::ANON_BATCH_SOFT_CAP,
            anon_daily: crate::constants::ANON_DAILY_LOOKUPS,
            free_daily: crate::constants::FREE_DAILY_LOOKUPS,
        }
    }

    #[test]
    fn anonymous_audit_refuses_above_the_measured_soft_cap() {
        let cap = crate::constants::ANON_BATCH_SOFT_CAP;
        assert!(audit_quota_refusal(&quota(cap, false, None)).is_none());
        let refused = audit_quota_refusal(&quota(cap + 1, false, None)).unwrap();
        assert!(
            refused.contains(&format!("audit of {} DOIs", cap + 1)),
            "{refused}"
        );
        assert!(refused.contains(&format!("soft cap {cap}")), "{refused}");
        assert!(refused.contains("does not skip this audit"), "{refused}");
    }

    #[test]
    fn signed_in_audit_refuses_only_past_remaining_lookups() {
        assert!(audit_quota_refusal(&quota(2, true, Some(2))).is_none());
        let refused = audit_quota_refusal(&quota(1, true, Some(0))).unwrap();
        assert!(refused.contains("only 0 remaining"), "{refused}");
        assert!(refused.contains("plan free limit 60/day"), "{refused}");
        let cap = crate::constants::ANON_BATCH_SOFT_CAP;
        assert!(audit_quota_refusal(&quota(cap, true, None)).is_none());
        let unconfirmed = audit_quota_refusal(&quota(cap + 1, true, None)).unwrap();
        assert!(
            unconfirmed.contains("quota was not confirmed"),
            "{unconfirmed}"
        );
        assert!(
            unconfirmed.contains(&format!("soft cap {cap}")),
            "{unconfirmed}"
        );
        assert!(
            unconfirmed.contains("does not skip this audit"),
            "{unconfirmed}"
        );
    }

    #[test]
    fn parser_reads_braces_quotes_urls_and_skips_directives() {
        let bib = r#"
            @string{jan = {January}}
            @comment{not an entry}
            @comment this line is also not an entry
            % a comment between entries
            @article{clean,
              title = {{RMSD} projections},
              author = {Goswami, Rohit and {National Institute of Standards and Technology}},
              year = 2024,
              journal = "MethodsX",
              number = {16},
              pages = {1--10},
              doi = {https://doi.org/10.1016/j.mex.2024.1}
            }
            @inproceedings(quoted,
              title = {Hello} # { World},
              url = {https://dx.doi.org/10.1109/example.1}
            )
        "#;
        let entries = parse_bibtex(bib).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title.as_deref(), Some("RMSD projections"));
        assert_eq!(
            entries[0].authors,
            vec![
                "Goswami, Rohit".to_string(),
                "National Institute of Standards and Technology".to_string()
            ]
        );
        assert_eq!(entries[0].year, Some(2024));
        assert_eq!(entries[0].journal.as_deref(), Some("MethodsX"));
        assert_eq!(entries[0].issue.as_deref(), Some("16"));
        assert_eq!(entries[0].pages.as_deref(), Some("1--10"));
        assert_eq!(entries[0].doi.as_deref(), Some("10.1016/j.mex.2024.1"));
        assert_eq!(entries[1].title.as_deref(), Some("Hello World"));
        assert_eq!(entries[1].doi.as_deref(), Some("10.1109/example.1"));
    }

    #[test]
    fn a_doi_field_that_is_not_a_doi_is_kept_for_the_report() {
        let entries = parse_bibtex("@article{bad, doi = {not-a-doi}}").unwrap();
        assert!(entries[0].doi.is_none());
        assert_eq!(entries[0].malformed_doi.as_deref(), Some("not-a-doi"));
    }

    #[test]
    fn measured_fixture_catalogue_parses_entry_for_entry() {
        let bib = include_str!("../tests/fixtures/readcon-cpc.bib");
        let declared = bib.lines().filter(|line| line.starts_with('@')).count();
        let entries = parse_bibtex(bib).unwrap();
        assert_eq!(
            entries.len(),
            declared,
            "parser dropped or invented entries"
        );
        assert!(declared > 0);
        let sample = entries
            .iter()
            .find(|entry| entry.key == "goswamiReadconcoreConConvel2026")
            .unwrap();
        assert_eq!(sample.doi.as_deref(), Some("10.5281/zenodo.18753567"));
        assert_eq!(sample.year, Some(2026));
        assert!(entries
            .iter()
            .filter_map(|entry| entry.doi.as_deref())
            .all(looks_like_doi_token));
    }

    #[test]
    fn crossref_update_to_and_relation_and_title_are_notices() {
        let provider = json!({
            "crossref": {
                "title": ["RETRACTED: A result"],
                "update-to": [{
                    "DOI": "10.5555/notice",
                    "type": "retraction",
                    "updated": {"date-parts": [[2021, 6, 2]]}
                }, {
                    "DOI": "10.5555/fix",
                    "type": "erratum",
                    "updated": {"date-parts": [[2020]]}
                }],
                "relation": {
                    "is-retracted-by": [{"id-type": "doi", "id": "10.5555/notice"}]
                }
            }
        });
        let notices = notices_from_provider(Some(&provider));
        assert!(notices.iter().any(|notice| {
            notice.kind == NoticeKind::Retraction
                && notice.doi.as_deref() == Some("10.5555/notice")
                && notice.date.as_deref() == Some("2021-06-02")
        }));
        assert!(notices
            .iter()
            .any(|notice| notice.kind == NoticeKind::Correction
                && notice.date.as_deref() == Some("2020")));
        assert!(notices
            .iter()
            .any(|notice| notice.kind == NoticeKind::Retraction && notice.from_title));
        assert_eq!(
            notices
                .iter()
                .filter(
                    |notice| notice.doi.as_deref() == Some("10.5555/notice") && !notice.from_title
                )
                .count(),
            1,
            "the same retraction DOI from update-to and relation is one notice"
        );
    }

    #[test]
    fn datacite_is_retracted_by_is_a_retraction_and_retracts_is_not() {
        let provider = json!({
            "datacite": {
                "titles": [{"title": "A deposited record"}],
                "relatedIdentifiers": [
                    {"relationType": "IsRetractedBy", "relatedIdentifier": "https://doi.org/10.1234/notice"},
                    {"relationType": "Retracts", "relatedIdentifier": "10.1234/original"}
                ]
            }
        });
        let notices = notices_from_provider(Some(&provider));
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].kind, NoticeKind::Retraction);
        assert_eq!(notices[0].doi.as_deref(), Some("10.1234/notice"));
    }

    #[test]
    fn withdrawal_and_concern_keep_their_own_status() {
        let bib = parse_bibtex(
            "@article{w, title={Left}, year={2019}, doi={10.1/w}}
             @article{c, title={Left}, year={2019}, doi={10.1/c}}",
        )
        .unwrap();
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "10.1/w".into(),
            resolved(json!({"title": "Left", "date": {"year": 2019}}), json!({
                "crossref": {"update-to": [{"type": "withdrawal", "DOI": "10.1/wd", "updated": {"date-parts": [[2019, 3]]}}]}
            })),
        );
        outcomes.insert(
            "10.1/c".into(),
            resolved(
                json!({"title": "Left", "date": {"year": 2019}}),
                json!({
                    "crossref": {"update-to": [{"type": "expression_of_concern", "DOI": "10.1/cn"}]}
                }),
            ),
        );
        let report = render_audit(&bib, &outcomes);
        assert!(report.contains("1 WITHDRAWN, 1 CONCERN."), "{report}");
        assert!(
            report.contains("withdrawn by doi:10.1/wd on 2019-03"),
            "{report}"
        );
        assert!(
            report.contains("expression of concern doi:10.1/cn"),
            "{report}"
        );
        assert!(!report.contains("RETRACTED"), "{report}");
    }

    #[test]
    fn a_correction_stays_ok_and_a_mismatch_is_not_called_retracted() {
        let bib = parse_bibtex(
            "@article{fixed, title={Left}, year={2020}, doi={10.1/fixed}}
             @article{wrong, title={Left}, year={1989}, doi={10.1/wrong}}
             @article{bare, title={Local note}}
             @article{bad, doi={soon}}",
        )
        .unwrap();
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "10.1/fixed".into(),
            resolved(
                json!({"title": "Left", "date": {"year": 2020}}),
                json!({
                    "crossref": {"update-to": [{"type": "correction", "DOI": "10.1/erratum"}]}
                }),
            ),
        );
        outcomes.insert(
            "10.1/wrong".into(),
            resolved(
                json!({"title": "Left", "date": {"year": 1993}}),
                json!({"crossref": {}}),
            ),
        );
        let report = render_audit(&bib, &outcomes);
        assert!(
            report.starts_with(
                "Audited 4 entries, 2 with a DOI.\n1 OK, 1 BROKEN, 1 MISMATCH, 1 NO DOI."
            ),
            "{report}"
        );
        assert!(report.contains("correction doi:10.1/erratum"), "{report}");
        assert!(
            report.contains("year: claimed 1989, record 1993"),
            "{report}"
        );
        assert!(report.contains("doi field is not a DOI: soon"), "{report}");
        assert!(report.contains("no DOI in the entry"), "{report}");
        let named: usize = report
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap_or(0);
        assert_eq!(named, 0, "the total line is 'Audited N', not a bare count");
        let counts = status_counts(&report);
        assert_eq!(counts.iter().sum::<usize>(), 4);
    }

    #[test]
    fn a_manuscript_title_prefix_does_not_retract_the_entry() {
        let bib = parse_bibtex(
            "@article{marked, title={RETRACTED: Left}, year={2020}, doi={10.1/marked}}",
        )
        .unwrap();
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "10.1/marked".into(),
            resolved(
                json!({"title": "RETRACTED: Left", "date": {"year": 2020}}),
                json!({"crossref": {"title": ["Left"]}}),
            ),
        );
        let report = render_audit(&bib, &outcomes);
        assert!(report.contains("| OK |"), "{report}");
        assert!(!report.contains("| RETRACTED |"), "{report}");
        assert!(
            report.contains(
                "manuscript title is marked retracted; the publisher record does not say so"
            ),
            "{report}"
        );
    }

    #[test]
    fn bibliography_limits_refuse_bytes_entries_and_long_fields() {
        let bytes = "x".repeat(crate::constants::AUDIT_BIB_MAX_BYTES + 1);
        let too_big = parse_bibtex(&bytes).unwrap_err();
        assert!(
            too_big.contains(&format!(
                "the limit is {}",
                crate::constants::AUDIT_BIB_MAX_BYTES
            )),
            "{too_big}"
        );

        let mut entries = String::new();
        for n in 0..=crate::constants::AUDIT_BIB_MAX_ENTRIES {
            entries.push_str(&format!("@article{{k{n}, title={{T}}}}\n"));
        }
        let too_many = parse_bibtex(&entries).unwrap_err();
        assert!(
            too_many.contains(&format!(
                "more than {} entries",
                crate::constants::AUDIT_BIB_MAX_ENTRIES
            )),
            "{too_many}"
        );

        let field = "a".repeat(crate::constants::AUDIT_BIB_MAX_FIELD_BYTES + 1);
        let too_long = parse_bibtex(&format!("@article{{k, title={{{field}}}}}")).unwrap_err();
        assert!(
            too_long.contains(&format!(
                "the limit is {}",
                crate::constants::AUDIT_BIB_MAX_FIELD_BYTES
            )),
            "{too_long}"
        );
    }

    #[test]
    fn no_publisher_record_is_unchecked_even_when_the_fields_agree() {
        let bib = parse_bibtex("@article{x, title={Left}, doi={10.1/x}}").unwrap();
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "10.1/x".into(),
            DoiOutcome::Resolved {
                metadata: json!({"title": "Left", "doi": "10.1/x"}),
                provider: None,
                provider_error: None,
            },
        );
        let report = render_audit(&bib, &outcomes);
        assert!(report.contains("| UNCHECKED |"), "{report}");
        assert!(
            report.contains("no Crossref or DataCite record"),
            "{report}"
        );
        assert!(!report.contains("| OK |"), "{report}");
    }

    fn resolved(metadata: Value, provider: Value) -> DoiOutcome {
        DoiOutcome::Resolved {
            metadata,
            provider: Some(provider),
            provider_error: None,
        }
    }

    fn status_counts(report: &str) -> Vec<usize> {
        let line = report.lines().nth(1).unwrap().trim_end_matches('.');
        line.split(", ")
            .map(|part| {
                part.split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .collect()
    }
}
