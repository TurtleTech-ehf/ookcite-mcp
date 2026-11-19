//! Source bibliography entries kept across import and export.
//!
//! The OokCite API does not yet store every BibTeX field, and a resolver
//! value must not replace a value the file already has. On a single-user
//! server the import writes one [`BibSource`] per entry into the local
//! override file. Export prints that record.
//!
//! What is normalised, and nothing else:
//! - the entry type and field names are lower case;
//! - field order is the source order;
//! - each field is written as `  name = {value}`, with a comma between
//!   fields and no comma after the last;
//! - entries are separated by a blank line, and the text ends at the last
//!   entry's closing line;
//! - a numeric or quoted value is written in braces;
//! - `#` concatenation of braced text is joined. A value that names an
//!   `@string` macro is written bare, so `month = jan` stays `month = jan`;
//! - `doi`, `url`, and `eprint` are copied verbatim;
//! - a Unicode en dash or em dash is written `--` or `---`;
//! - a bare `&`, `%`, `#`, or `_` in any other field is escaped, and an
//!   escaped one is not escaped again.
//!
//! Braces, `--`, name particles, and LaTeX accents are copied as written.
//! A citation key is kept. A key is generated only when the source entry
//! has none. A repeated key keeps the first spelling and then takes the
//! suffixes `a`, `b`, `c`, through `z`, then `aa` through `zz`. One more
//! collision is an error. Resolver fields are not added unless export is
//! asked to enrich, and then only a field the source left empty and the
//! caller did not clear.

#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bib_audit::{parse_bibtex_raw, RawBibEntry};
use crate::bib_names;
use crate::collection_entries::{entry_doi, normalize_doi_token};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BibSource {
    pub key: String,
    #[serde(default)]
    pub entry_type: String,
    #[serde(default)]
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedSource {
    pub entry_id: Option<String>,
    pub doi: Option<String>,
    pub source: BibSource,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BibliographyDiff {
    pub source_entries: usize,
    pub export_entries: usize,
    pub key_changes: usize,
    pub type_changes: usize,
    pub entries_differing: usize,
    pub dropped_fields: usize,
    pub added_fields: usize,
    pub value_changes: usize,
}

impl BibSource {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| bare_expr(value).unwrap_or(value.as_str()))
    }

    pub fn set_field(&mut self, name: &str, value: String) {
        let name = name.to_ascii_lowercase();
        if let Some(existing) = self
            .fields
            .iter_mut()
            .find(|(field, _)| field.eq_ignore_ascii_case(&name))
        {
            existing.1 = value;
        } else {
            self.fields.push((name, value));
        }
    }

    pub fn remove_field(&mut self, name: &str) {
        self.fields
            .retain(|(field, _)| !field.eq_ignore_ascii_case(name));
    }
}

pub fn source_from_meta(meta: &Value) -> Option<BibSource> {
    serde_json::from_value(meta.get("bib_source")?.clone()).ok()
}

/// Parse a bibliography and attach each entry to the server record it became.
///
/// A normalized DOI matches first. What is left matches one record by arXiv or
/// eprint id, then by ISBN, then by the original citation key. A title and year
/// match only when both are present and that pair hits one record on each side.
/// An entry that matches nothing is still returned, with no entry id, so its
/// source snapshot stays.
pub fn pair_with_server(
    content: &str,
    server: &[Value],
) -> Result<Vec<PairedSource>, BibliographyError> {
    let entries = parse_bibtex_raw(content).map_err(BibliographyError::Parse)?;
    let mut used = vec![false; server.len()];
    let mut paired: Vec<Option<usize>> = vec![None; entries.len()];
    for (index, entry) in entries.iter().enumerate() {
        let Some(doi) = source_doi(entry) else {
            continue;
        };
        if let Some(pos) = server.iter().enumerate().position(|(pos, record)| {
            !used[pos] && server_doi(record).as_deref() == Some(doi.as_str())
        }) {
            used[pos] = true;
            paired[index] = Some(pos);
        }
    }
    let mut source_left: Vec<usize> = paired
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| slot.is_none().then_some(index))
        .collect();
    let mut server_left: Vec<usize> = used
        .iter()
        .enumerate()
        .filter_map(|(index, taken)| (!taken).then_some(index))
        .collect();
    // Identifiers before a title. A match has to be unique on both sides.
    take_unique_matches(
        &entries,
        server,
        &mut source_left,
        &mut server_left,
        &mut paired,
        |entry, record| same_id(source_eprint(entry), server_eprint(record)),
    );
    take_unique_matches(
        &entries,
        server,
        &mut source_left,
        &mut server_left,
        &mut paired,
        |entry, record| same_id(source_isbn(entry), server_isbn(record)),
    );
    take_unique_matches(
        &entries,
        server,
        &mut source_left,
        &mut server_left,
        &mut paired,
        |entry, record| {
            let key = entry.key.trim();
            !key.is_empty()
                && server_citation_key(record).is_some_and(|have| have.eq_ignore_ascii_case(key))
        },
    );
    take_unique_matches(
        &entries,
        server,
        &mut source_left,
        &mut server_left,
        &mut paired,
        |entry, record| {
            let title = loose_title(field_raw(entry, "title").unwrap_or(""));
            if title.is_empty() || loose_title(&server_title(record)) != title {
                return false;
            }
            match (entry_year(entry), server_year(record)) {
                (Some(source_year), Some(record_year)) => source_year == record_year,
                _ => false,
            }
        },
    );
    let mut seen_keys = HashSet::new();
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let source = to_source(entry, &mut seen_keys)?;
        let Some(server_index) = paired[index] else {
            out.push(PairedSource {
                entry_id: None,
                doi: source_doi(entry),
                source,
            });
            continue;
        };
        let record = &server[server_index];
        out.push(PairedSource {
            entry_id: record
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            doi: server_doi(record).or_else(|| source_doi(entry)),
            source,
        });
    }
    Ok(out)
}

fn take_unique_matches(
    entries: &[RawBibEntry],
    server: &[Value],
    source_left: &mut Vec<usize>,
    server_left: &mut Vec<usize>,
    paired: &mut [Option<usize>],
    matches: impl Fn(&RawBibEntry, &Value) -> bool,
) {
    let sources: Vec<usize> = source_left.clone();
    let servers: Vec<usize> = server_left.clone();
    let hit_of: Vec<(usize, Option<usize>)> = sources
        .iter()
        .copied()
        .map(|source_index| {
            (
                source_index,
                single_hit(&entries[source_index], server, &servers, &matches),
            )
        })
        .collect();
    for (source_index, hit) in &hit_of {
        let Some(server_index) = *hit else {
            continue;
        };
        let claims = hit_of
            .iter()
            .filter(|(_, other)| *other == Some(server_index))
            .count();
        if claims != 1 {
            continue;
        }
        paired[*source_index] = Some(server_index);
        source_left.retain(|index| *index != *source_index);
        server_left.retain(|index| *index != server_index);
    }
}

/// The one server record `entry` matches, or nothing when it matches several.
fn single_hit(
    entry: &RawBibEntry,
    server: &[Value],
    servers: &[usize],
    matches: &impl Fn(&RawBibEntry, &Value) -> bool,
) -> Option<usize> {
    let mut found = None;
    for pos in servers {
        if !matches(entry, &server[*pos]) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(*pos);
    }
    found
}

fn same_id(left: Option<String>, right: Option<String>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn source_eprint(entry: &RawBibEntry) -> Option<String> {
    field_raw(entry, "eprint")
        .or_else(|| field_raw(entry, "arxiv"))
        .and_then(normalize_eprint)
}

fn server_eprint(record: &Value) -> Option<String> {
    meta_str(record, "eprint")
        .or_else(|| meta_str(record, "arxiv"))
        .and_then(normalize_eprint)
}

fn normalize_eprint(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let id = lower.strip_prefix("arxiv:").unwrap_or(lower.as_str());
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn source_isbn(entry: &RawBibEntry) -> Option<String> {
    field_raw(entry, "isbn").and_then(normalize_isbn)
}

fn server_isbn(record: &Value) -> Option<String> {
    meta_str(record, "isbn").and_then(normalize_isbn)
}

fn normalize_isbn(raw: &str) -> Option<String> {
    let folded: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_uppercase())
        .collect();
    if folded.is_empty() {
        None
    } else {
        Some(folded)
    }
}

fn meta_str<'a>(record: &'a Value, name: &str) -> Option<&'a str> {
    let meta = record.get("metadata").unwrap_or(record);
    meta.get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn server_citation_key(record: &Value) -> Option<String> {
    let meta = record.get("metadata").unwrap_or(record);
    meta.get("citation_key")
        .or_else(|| meta.get("cite_key"))
        .or_else(|| record.pointer("/render/citation_key"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
}

fn server_title(record: &Value) -> String {
    let meta = record.get("metadata").unwrap_or(record);
    meta.get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn server_year(record: &Value) -> Option<i32> {
    let meta = record.get("metadata").unwrap_or(record);
    if let Some(year) = meta
        .get("year")
        .and_then(Value::as_i64)
        .or_else(|| meta.pointer("/date/year").and_then(Value::as_i64))
    {
        return i32::try_from(year)
            .ok()
            .filter(|year| (1000..3000).contains(year));
    }
    meta.get("year")
        .and_then(Value::as_str)
        .and_then(first_year)
}

fn entry_year(entry: &RawBibEntry) -> Option<i32> {
    field_raw(entry, "year").and_then(first_year)
}

fn loose_title(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch == '{' || ch == '}' {
            continue;
        }
        if ch.is_whitespace() {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
pub fn render_bibliography(entries: &[BibSource]) -> Result<String, CitationKeyExhausted> {
    let mut used = HashSet::new();
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&render_entry(entry, &mut used)?);
    }
    Ok(out)
}

pub fn render_entry(
    entry: &BibSource,
    used: &mut HashSet<String>,
) -> Result<String, CitationKeyExhausted> {
    let mut key = entry.key.trim().to_string();
    if key.is_empty() {
        key = generate_key(entry);
    }
    if key.is_empty() {
        key = "ref".into();
    }
    key = dedup_key(key, used)?;
    let entry_type = entry.entry_type.trim().to_ascii_lowercase();
    let entry_type = if entry_type.is_empty() {
        "misc"
    } else {
        entry_type.as_str()
    };
    let mut lines = Vec::new();
    for (name, value) in &entry.fields {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || name == "citation_key" || name == "entry_type" {
            continue;
        }
        if let Some(line) = format_field_line(&name, value) {
            lines.push(line);
        }
    }
    Ok(format!(
        "@{entry_type}{{{key},\n{}\n}}\n",
        lines.join(",\n")
    ))
}

/// Fill only fields the source does not already have.
pub fn fill_empty(entry: &mut BibSource, additions: &[(String, String)]) {
    for (name, value) in additions {
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let present = entry
            .field(&name)
            .is_some_and(|have| !have.trim().is_empty());
        if !present {
            entry.set_field(&name, value.to_string());
        }
    }
}

pub fn prepare_field_value(value: &str) -> String {
    escape_latex_specials(&unicode_dashes_to_tex(value))
}

pub fn escape_latex_specials(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    for (index, ch) in chars.iter().enumerate() {
        if matches!(ch, '&' | '%' | '#' | '_') && (index == 0 || chars[index - 1] != '\\') {
            out.push('\\');
        }
        out.push(*ch);
    }
    out
}

pub fn unicode_dashes_to_tex(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '—' | '―' => out.push_str("---"),
            '–' | '‒' => out.push_str("--"),
            _ => out.push(ch),
        }
    }
    out
}

/// Field and type differences between two bibliographies.
///
/// Entries are aligned by DOI, then by a brace-stripped title, then by
/// position. `entries_differing` counts an entry whose type or fields differ.
/// A key change alone does not. Values are compared after collapsing
/// whitespace. A source value is escaped, and its Unicode dashes are turned
/// into TeX dashes, before that comparison, except `doi`, `url`, `eprint`,
/// and a bare macro, which are compared as written.
#[cfg(test)]
pub fn diff_bibliographies(source: &str, exported: &str) -> Result<BibliographyDiff, String> {
    compare_bibliographies(source, exported, true)
}

#[cfg(test)]
fn compare_bibliographies(
    source: &str,
    exported: &str,
    canonical: bool,
) -> Result<BibliographyDiff, String> {
    let source_entries = parse_bibtex_raw(source)?;
    let export_entries = parse_bibtex_raw(exported)?;
    let export_of = align(&source_entries, &export_entries);
    let mut diff = BibliographyDiff {
        source_entries: source_entries.len(),
        export_entries: export_entries.len(),
        key_changes: 0,
        type_changes: 0,
        entries_differing: 0,
        dropped_fields: 0,
        added_fields: 0,
        value_changes: 0,
    };
    for (index, entry) in source_entries.iter().enumerate() {
        let Some(other) = export_of[index].and_then(|pos| export_entries.get(pos)) else {
            diff.key_changes += 1;
            diff.entries_differing += 1;
            diff.dropped_fields += entry.fields.len();
            continue;
        };
        let mut field_diff = false;
        if entry.key != other.key {
            diff.key_changes += 1;
        }
        if entry.entry_type != other.entry_type {
            diff.type_changes += 1;
            field_diff = true;
        }
        let source_fields = field_map(entry);
        let export_fields = field_map(other);
        for (name, value) in &source_fields {
            match export_fields.get(name) {
                None => {
                    diff.dropped_fields += 1;
                    field_diff = true;
                }
                Some(exported) if !values_equal(name, value, exported, canonical) => {
                    diff.value_changes += 1;
                    field_diff = true;
                }
                Some(_) => {}
            }
        }
        for name in export_fields.keys() {
            if !source_fields.contains_key(name) {
                diff.added_fields += 1;
                field_diff = true;
            }
        }
        if field_diff {
            diff.entries_differing += 1;
        }
    }
    Ok(diff)
}

#[cfg(test)]
fn values_equal(name: &str, source: &str, exported: &str, canonical: bool) -> bool {
    let source = collapse_whitespace(source);
    let exported = collapse_whitespace(exported);
    let verbatim = matches!(name, "doi" | "url" | "eprint") || bare_expr(&source).is_some();
    if canonical && !verbatim {
        prepare_field_value(&source) == exported
    } else {
        source == exported
    }
}

#[cfg(test)]
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
fn field_map(entry: &RawBibEntry) -> HashMap<String, String> {
    entry
        .fields
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect()
}

#[cfg(test)]
fn align(source: &[RawBibEntry], exported: &[RawBibEntry]) -> Vec<Option<usize>> {
    let mut used = vec![false; exported.len()];
    let mut paired = vec![None; source.len()];
    for (index, entry) in source.iter().enumerate() {
        let Some(doi) = source_doi(entry) else {
            continue;
        };
        if let Some(pos) = exported.iter().enumerate().position(|(pos, other)| {
            !used[pos] && source_doi(other).as_deref() == Some(doi.as_str())
        }) {
            used[pos] = true;
            paired[index] = Some(pos);
        }
    }
    for (index, entry) in source.iter().enumerate() {
        if paired[index].is_some() {
            continue;
        }
        let title = match_title(field_raw(entry, "title").unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let hits: Vec<usize> = exported
            .iter()
            .enumerate()
            .filter(|(pos, other)| {
                !used[*pos] && match_title(field_raw(other, "title").unwrap_or("")) == title
            })
            .map(|(pos, _)| pos)
            .collect();
        if hits.len() == 1 {
            used[hits[0]] = true;
            paired[index] = Some(hits[0]);
        }
    }
    for (index, slot) in paired.iter_mut().enumerate() {
        if slot.is_some() {
            continue;
        }
        if index < exported.len() && !used[index] {
            used[index] = true;
            *slot = Some(index);
        }
    }
    paired
}

#[cfg(test)]
fn match_title(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in value.chars() {
        match ch {
            '{' | '}' => {}
            '—' | '―' | '–' | '‒' | '-' => {
                if !dash {
                    out.push('-');
                    dash = true;
                }
            }
            ch if ch.is_whitespace() => {
                dash = false;
                out.push(' ');
            }
            ch => {
                dash = false;
                out.extend(ch.to_lowercase());
            }
        }
    }
    collapse_whitespace(&out)
}

fn to_source(
    entry: &RawBibEntry,
    used: &mut HashSet<String>,
) -> Result<BibSource, CitationKeyExhausted> {
    let mut key = entry.key.trim().to_string();
    if key.is_empty() {
        let draft = BibSource {
            key: String::new(),
            entry_type: entry.entry_type.clone(),
            fields: entry.fields.clone(),
        };
        key = generate_key(&draft);
    }
    if key.is_empty() {
        key = "ref".into();
    }
    key = dedup_key(key, used)?;
    Ok(BibSource {
        key,
        entry_type: entry.entry_type.clone(),
        fields: entry.fields.clone(),
    })
}

/// The first valid DOI among the `doi` field and then the `url` field.
fn source_doi(entry: &RawBibEntry) -> Option<String> {
    for name in ["doi", "url"] {
        if let Some(doi) = field_raw(entry, name).and_then(crate::bib_audit::clean_doi) {
            return Some(doi);
        }
    }
    None
}

fn server_doi(entry: &Value) -> Option<String> {
    let raw = entry_doi(entry)?;
    let normalized = normalize_doi_token(&raw);
    crate::bib_audit::clean_doi(&normalized).or(Some(normalized))
}

fn field_raw<'a>(entry: &'a RawBibEntry, name: &str) -> Option<&'a str> {
    entry
        .fields
        .iter()
        .find(|(field, _)| field.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn generate_key(entry: &BibSource) -> String {
    let author = entry.field("author").unwrap_or("");
    let family = bib_names::parse_list(author)
        .into_iter()
        .next()
        .map(|name| fold_key(&name.last))
        .filter(|family| !family.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let mut key = family;
    key.push_str(&title_key_words(entry.field("title").unwrap_or("")));
    if let Some(year) = entry.field("year").and_then(first_year) {
        key.push_str(&year.to_string());
    }
    key
}

fn title_key_words(title: &str) -> String {
    const STOP: &[&str] = &[
        "a", "an", "the", "of", "and", "for", "in", "on", "to", "with", "via", "from", "by", "or",
        "at", "as", "its",
    ];
    let plain = brace_text(title);
    let mut out = String::new();
    let mut count = 0;
    for raw in plain.split_whitespace() {
        if count == 3 {
            break;
        }
        let folded = fold_key(raw);
        if folded.is_empty() || STOP.contains(&folded.as_str()) {
            continue;
        }
        let mut chars = folded.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
            count += 1;
        }
    }
    out
}

fn brace_text(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            while chars.peek().is_some_and(|next| next.is_ascii_alphabetic()) {
                chars.next();
            }
            continue;
        }
        if ch != '{' && ch != '}' {
            out.push(ch);
        }
    }
    out
}

fn fold_key(value: &str) -> String {
    let plain = brace_text(value);
    let mut out = String::new();
    for ch in plain.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
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
    (1000..3000).contains(&year).then_some(year)
}

/// A stored value that must be emitted as a BibTeX expression, not wrapped in braces.
const BARE_VALUE: char = '\u{1}';

pub(crate) fn bare_value(expr: &str) -> String {
    format!("{BARE_VALUE}{expr}")
}

pub(crate) fn bare_expr(value: &str) -> Option<&str> {
    value.strip_prefix(BARE_VALUE)
}

fn format_field_line(name: &str, value: &str) -> Option<String> {
    if let Some(expr) = bare_expr(value) {
        let expr = expr.trim();
        if expr.is_empty() {
            return None;
        }
        return Some(format!("  {name} = {expr}"));
    }
    let verbatim = matches!(name, "doi" | "url" | "eprint");
    let value = if verbatim {
        value.trim().to_string()
    } else {
        prepare_field_value(value)
    };
    if value.trim().is_empty() {
        return None;
    }
    Some(format!("  {name} = {{{value}}}"))
}

/// Longest disambiguating suffix. `a`–`z`, then `aa`–`zz`, and then stop.
const MAX_SUFFIX_CHARS: usize = 2;

/// No suffix of `a`–`z` or `aa`–`zz` was free for `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationKeyExhausted {
    pub base: String,
}

impl std::fmt::Display for CitationKeyExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no free citation key for \"{}\" after suffixes a-z and aa-zz",
            self.base
        )
    }
}

impl std::error::Error for CitationKeyExhausted {}

/// A bibliography could not be paired or its citation keys ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BibliographyError {
    Parse(String),
    CitationKey(CitationKeyExhausted),
}

impl std::fmt::Display for BibliographyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => f.write_str(message),
            Self::CitationKey(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for BibliographyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(_) => None,
            Self::CitationKey(error) => Some(error),
        }
    }
}

impl From<CitationKeyExhausted> for BibliographyError {
    fn from(error: CitationKeyExhausted) -> Self {
        Self::CitationKey(error)
    }
}

/// Return `key` when it is free. Otherwise append `a`, `b`, `c`, through `z`, then `aa` through `zz`.
///
/// The first twenty-six collisions are the single letters this exporter already
/// wrote. A further collision past `zz` is [`CitationKeyExhausted`].
pub(crate) fn dedup_key(
    key: String,
    used: &mut HashSet<String>,
) -> Result<String, CitationKeyExhausted> {
    if used.insert(key.clone()) {
        return Ok(key);
    }
    let base = key;
    let mut suffix = String::new();
    while next_letter_suffix(&mut suffix) {
        let candidate = format!("{base}{suffix}");
        if used.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(CitationKeyExhausted { base })
}

/// Advance `suffix` through `a`–`z`, then `aa`–`zz`. Returns false at the cap.
fn next_letter_suffix(suffix: &mut String) -> bool {
    if suffix.is_empty() {
        suffix.push('a');
        return true;
    }
    let mut chars: Vec<char> = suffix.chars().collect();
    let mut index = chars.len();
    while index > 0 {
        index -= 1;
        if chars[index] != 'z' {
            let next = u32::from(chars[index]) + 1;
            chars[index] = char::from_u32(next).unwrap_or(chars[index]);
            *suffix = chars.into_iter().collect();
            return true;
        }
        chars[index] = 'a';
    }
    if chars.len() >= MAX_SUFFIX_CHARS {
        return false;
    }
    chars.insert(0, 'a');
    *suffix = chars.into_iter().collect();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(bib: &str) -> String {
        let entries = parse_bibtex_raw(bib).unwrap();
        let mut used = HashSet::new();
        let sources: Vec<BibSource> = entries
            .iter()
            .map(|entry| to_source(entry, &mut used))
            .collect::<Result<_, _>>()
            .unwrap();
        render_bibliography(&sources).unwrap()
    }

    #[test]
    fn source_key_is_kept_and_a_missing_key_is_generated() {
        let kept = round_trip("@article{voter2007,\n  title = {A title},\n  author = {Voter, Arthur F.},\n  year = {2007}\n}\n");
        assert!(kept.starts_with("@article{voter2007,"), "{kept}");
        assert!(!kept.contains("voterATitle2007"), "{kept}");
        let generated = round_trip("@article{,\n  title = {A title about atoms},\n  author = {van der Spoel, David},\n  year = {2001}\n}\n");
        assert!(generated.contains("@article{spoel"), "{generated}");
        assert!(!generated.contains("@article{,"), "{generated}");
    }

    #[test]
    fn dropped_source_fields_are_kept() {
        let bib = r#"@inproceedings{clinger1990,
  title = {How to read},
  booktitle = {Proceedings of the {ACM} {SIGPLAN}},
  howpublished = {Zenodo},
  note = {Accessed 2026-11-17},
  version = {1.1.0},
  institution = {IETF},
  address = {Hoboken, NJ},
  type = {RFC},
  edition = {Second},
  series = {NATO Science Series},
  organization = {JCGM},
  editor = {{Zarr Developers}}
}
"#;
        let out = round_trip(bib);
        for field in [
            "booktitle = {Proceedings of the {ACM} {SIGPLAN}}",
            "howpublished = {Zenodo}",
            "note = {Accessed 2026-11-17}",
            "version = {1.1.0}",
            "institution = {IETF}",
            "address = {Hoboken, NJ}",
            "type = {RFC}",
            "edition = {Second}",
            "series = {NATO Science Series}",
            "organization = {JCGM}",
            "editor = {{Zarr Developers}}",
        ] {
            assert!(out.contains(field), "missing {field}\n{out}");
        }
    }

    #[test]
    fn source_entry_types_are_not_rewritten() {
        let bib = r#"@manual{h5md110,
  title = {{H5MD}},
  author = {{H5MD developers}},
  version = {1.1.0}
}
@incollection{voter2007,
  title = {Introduction to the kinetic {Monte} {Carlo} method},
  booktitle = {Radiation Effects in Solids},
  series = {NATO Science Series}
}
@misc{ieee754,
  title = {{IEEE} Standard},
  author = {{IEEE Computer Society}}
}
@techreport{bray2017,
  title = {The {JSON} format},
  institution = {IETF}
}
@phdthesis{student2020,
  title = {A thesis}
}
@software{chemfiles2023,
  title = {chemfiles}
}
@online{docs2026,
  title = {Getting started}
}
"#;
        let out = round_trip(bib);
        for header in [
            "@manual{h5md110,",
            "@incollection{voter2007,",
            "@misc{ieee754,",
            "@techreport{bray2017,",
            "@phdthesis{student2020,",
            "@software{chemfiles2023,",
            "@online{docs2026,",
        ] {
            assert!(out.contains(header), "missing {header}\n{out}");
        }
        assert!(!out.contains("@inbook"), "{out}");
    }

    #[test]
    fn braces_dashes_and_names_are_copied() {
        let bib = r#"@article{sample,
  title = {{IRA}: A Shape Matching Approach -- with {Monte} {Carlo}},
  author = {van der Spoel, David and de Buyl, Pierre and in 't Veld, Pieter J. and Steele, Jr., Guy L. and De Souza, Roger A. and Le Bris, Claude and S{\o}rensen, M. R. and B{\'e}land, Laurent Karim and Pober{\v{z}}nik, Matic and {Joint Committee for Guides in Metrology}},
  editor = {{Zarr Developers} and van der Walt, St{\'e}fan J.},
  pages = {5446--5457},
  number = {5--6},
  publisher = {Society for Industrial \& Applied Mathematics}
}
"#;
        let out = round_trip(bib);
        assert!(
            out.contains("title = {{IRA}: A Shape Matching Approach -- with {Monte} {Carlo}}"),
            "{out}"
        );
        assert!(out.contains("van der Spoel, David"), "{out}");
        assert!(out.contains("de Buyl, Pierre"), "{out}");
        assert!(out.contains("in 't Veld, Pieter J."), "{out}");
        assert!(out.contains("Steele, Jr., Guy L."), "{out}");
        assert!(out.contains("De Souza, Roger A."), "{out}");
        assert!(out.contains("Le Bris, Claude"), "{out}");
        assert!(!out.contains("Souza, Roger A. De"), "{out}");
        assert!(!out.contains("Bris, Claude Le"), "{out}");
        assert!(out.contains("S{\\o}rensen, M. R."), "{out}");
        assert!(!out.contains("So/rensen"), "{out}");
        assert!(!out.contains('ø'), "{out}");
        assert!(out.contains("B{\\'e}land, Laurent Karim"), "{out}");
        assert!(!out.contains("B{\\'e}land, L."), "{out}");
        assert!(out.contains("Pober{\\v{z}}nik, Matic"), "{out}");
        assert!(
            out.contains("{Joint Committee for Guides in Metrology}"),
            "{out}"
        );
        assert!(
            out.contains("editor = {{Zarr Developers} and van der Walt, St{\\'e}fan J.}"),
            "{out}"
        );
        assert!(out.contains("pages = {5446--5457}"), "{out}");
        assert!(out.contains("number = {5--6}"), "{out}");
        assert!(
            out.contains("publisher = {Society for Industrial \\& Applied Mathematics}"),
            "{out}"
        );
        assert!(!out.contains("\\\\&"), "{out}");
        assert!(!out.contains('–'), "{out}");
    }

    #[test]
    fn a_bare_ampersand_is_escaped_once() {
        let out = round_trip("@book{higham,\n  publisher = {Society for Industrial & Applied Mathematics},\n  title = {Accuracy and Stability}\n}\n");
        assert!(
            out.contains("publisher = {Society for Industrial \\& Applied Mathematics}"),
            "{out}"
        );
        assert!(!out.contains("\\\\&"), "{out}");
        let twice = prepare_field_value(r"Society for Industrial \& Applied Mathematics");
        assert_eq!(twice, r"Society for Industrial \& Applied Mathematics");
    }

    #[test]
    fn ascii_hyphen_pages_are_not_rewritten() {
        let out = round_trip("@inproceedings{thakur,\n  pages = {23-32},\n  title = {MPI-IO}\n}\n");
        assert!(out.contains("pages = {23-32}"), "{out}");
        assert!(!out.contains("23--32"), "{out}");
    }

    #[test]
    fn enrichment_fills_only_an_empty_field() {
        let mut source = BibSource {
            key: "k".into(),
            entry_type: "article".into(),
            fields: vec![
                ("title".into(), "Source title".into()),
                ("pages".into(), "1--5".into()),
            ],
        };
        fill_empty(
            &mut source,
            &[
                ("title".into(), "Resolver title".into()),
                ("volume".into(), "74".into()),
                ("pages".into(), "9--9".into()),
                ("issn".into(), "0000-0000".into()),
            ],
        );
        assert_eq!(source.field("title"), Some("Source title"));
        assert_eq!(source.field("pages"), Some("1--5"));
        assert_eq!(source.field("volume"), Some("74"));
        assert_eq!(source.field("issn"), Some("0000-0000"));
    }

    #[test]
    fn a_canonical_entry_is_byte_for_byte() {
        let bib = "\
@article{sample2001,
  title = {{IRA}: A shape with {Monte} {Carlo}},
  author = {van der Spoel, David and De Souza, Roger A.},
  year = {2001},
  journal = {The Journal of Chemical Physics},
  volume = {115},
  number = {5--6},
  pages = {5446--5457},
  doi = {10.1063/1.1}
}
";
        assert_eq!(round_trip(bib), bib);
    }

    #[test]
    fn pairing_uses_the_doi_and_keeps_an_unmatched_page() {
        let bib = "\
@article{kept,
  title = {Real title},
  doi = {10.1000/Kept}
}
@manual{web,
  title = {A page},
  url = {https://example.org/page}
}
";
        let server = vec![
            json!({"id": "e2", "metadata": {"title": "Resolver page"}}),
            json!({"id": "e1", "metadata": {"doi": "10.1000/kept", "title": "Other title", "entry_type": "misc"}}),
        ];
        let paired = pair_with_server(bib, &server).unwrap();
        assert_eq!(paired.len(), 2);
        let by_key = |key: &str| paired.iter().find(|item| item.source.key == key).unwrap();
        assert_eq!(by_key("kept").entry_id.as_deref(), Some("e1"));
        assert_eq!(by_key("kept").source.field("title"), Some("Real title"));
        let web = by_key("web");
        assert_eq!(web.entry_id, None);
        assert_eq!(web.doi, None);
        assert_eq!(web.source.entry_type, "manual");
        assert_eq!(web.source.field("title"), Some("A page"));
        assert_eq!(web.source.field("url"), Some("https://example.org/page"));
    }

    #[test]
    fn pairing_follows_identifiers_when_the_server_order_differs() {
        let bib = "\
@article{doiPaper,
  title = {Doi title},
  year = {2020},
  doi = {10.1000/ABC},
  eprint = {1111.11111}
}
@misc{preprint,
  title = {Preprint title},
  year = {2019},
  eprint = {arXiv:cond-mat/9801005},
  arxiv = {1901.99999}
}
@book{book,
  title = {Book title},
  year = {2018},
  isbn = {0-8044-2957-X}
}
@book{otherbook,
  title = {Second book title},
  year = {2004},
  isbn = {978-0-521-85629-7}
}
@misc{note2021,
  title = {Local note},
  year = {2017}
}
@manual{web,
  title = {A page},
  year = {2016},
  url = {https://example.org/page}
}
";
        let server = vec![
            json!({"id": "s-otherbook", "metadata": {"isbn": "9780521856297", "title": "Server second book", "year": 1980}}),
            json!({"id": "s-note", "metadata": {"citation_key": "note2021", "title": "Server note", "year": 1990}}),
            json!({"id": "s-other", "metadata": {"title": "Resolver page", "year": 2001, "eprint": "1111.11111"}}),
            json!({"id": "s-book", "isbn": "080442957x", "title": "Other book", "year": 1991}),
            json!({
                "id": "s-pre",
                "metadata": {
                    "eprint": "cond-mat/9801005",
                    "arxiv": "1901.99999",
                    "title": "Other preprint",
                    "year": 1992
                }
            }),
            json!({"id": "s-doi", "metadata": {"doi": "10.1000/abc", "eprint": "9999.99999", "title": "Other article", "year": 1993}}),
        ];
        let paired = pair_with_server(bib, &server).unwrap();
        assert_eq!(paired.len(), 6, "{paired:?}");
        let by_key = |key: &str| paired.iter().find(|item| item.source.key == key).unwrap();
        assert_eq!(by_key("doiPaper").entry_id.as_deref(), Some("s-doi"));
        assert_eq!(by_key("preprint").entry_id.as_deref(), Some("s-pre"));
        assert_eq!(by_key("book").entry_id.as_deref(), Some("s-book"));
        assert_eq!(by_key("otherbook").entry_id.as_deref(), Some("s-otherbook"));
        assert_eq!(by_key("note2021").entry_id.as_deref(), Some("s-note"));
        let web = by_key("web");
        assert_eq!(web.entry_id, None);
        assert_eq!(web.source.field("title"), Some("A page"));
        assert_eq!(web.source.field("url"), Some("https://example.org/page"));
    }

    #[test]
    fn duplicate_or_ambiguous_matches_stay_unpaired() {
        let bib = "\
@article{unique,
  title = {Unique preprint},
  year = {2018},
  arxiv = {1801.00001}
}
@book{copya,
  title = {Copy A},
  year = {2016},
  isbn = {9780262033848}
}
@book{copyb,
  title = {Copy B},
  year = {2016},
  isbn = {978-026-203-3848}
}
@article{ambiguous,
  title = {Same title},
  year = {2015}
}
@article{noyear,
  title = {Missing year},
  year = {2014}
}
@misc{blank,
  year = {2013}
}
@misc{blankid,
  title = {Blank id},
  year = {2011},
  eprint = {arxiv:},
  isbn = {---}
}
";
        let server = vec![
            json!({"id": "s-unique", "metadata": {"arxiv": "1801.00001", "title": "Elsewhere", "year": 2000}}),
            json!({"id": "s-isbn", "metadata": {"isbn": "9780262033848", "title": "Elsewhere", "year": 2000}}),
            json!({"id": "s-amb1", "metadata": {"title": "Same title", "year": 2015}}),
            json!({"id": "s-amb2", "metadata": {"title": "Same title", "year": 2015}}),
            json!({"id": "s-noyear", "metadata": {"title": "Missing year"}}),
            json!({"id": "s-blank", "metadata": {"year": 2013}}),
            json!({"id": "s-blankid", "metadata": {"eprint": "arxiv:", "isbn": "---", "title": "Not blank", "year": 2001}}),
        ];
        let paired = pair_with_server(bib, &server).unwrap();
        assert_eq!(paired.len(), 7, "{paired:?}");
        let by_key = |key: &str| paired.iter().find(|item| item.source.key == key).unwrap();
        assert_eq!(by_key("unique").entry_id.as_deref(), Some("s-unique"));
        for key in ["copya", "copyb", "ambiguous", "noyear", "blank", "blankid"] {
            let item = by_key(key);
            assert_eq!(item.entry_id, None, "{key}: {item:?}");
        }
        assert_eq!(by_key("copya").source.field("title"), Some("Copy A"));
        assert_eq!(
            by_key("copyb").source.field("isbn"),
            Some("978-026-203-3848")
        );
        assert_eq!(
            by_key("ambiguous").source.field("title"),
            Some("Same title")
        );
        assert_eq!(by_key("noyear").source.field("year"), Some("2014"));
        assert_eq!(by_key("blank").source.field("year"), Some("2013"));
        assert_eq!(by_key("blankid").source.field("eprint"), Some("arxiv:"));
    }

    #[test]
    fn a_malformed_doi_uses_the_doi_in_the_url() {
        let bib = "\
@article{fromurl,
  title = {From the url},
  year = {2020},
  doi = {not-a-doi},
  url = {https://doi.org/10.1000/from-url}
}
@article{fromfield,
  title = {From the field},
  year = {2021},
  doi = {10.1000/From-Field},
  url = {https://doi.org/10.1000/other}
}
";
        let server = vec![
            json!({"id": "s-field", "metadata": {"doi": "10.1000/from-field", "title": "Elsewhere", "year": 1991}}),
            json!({"id": "s-url", "metadata": {"doi": "10.1000/from-url", "title": "Elsewhere", "year": 1990}}),
        ];
        let paired = pair_with_server(bib, &server).unwrap();
        assert_eq!(paired.len(), 2, "{paired:?}");
        let by_key = |key: &str| paired.iter().find(|item| item.source.key == key).unwrap();
        assert_eq!(by_key("fromurl").entry_id.as_deref(), Some("s-url"));
        assert_eq!(by_key("fromurl").doi.as_deref(), Some("10.1000/from-url"));
        assert_eq!(by_key("fromfield").entry_id.as_deref(), Some("s-field"));
        assert_eq!(
            by_key("fromfield").doi.as_deref(),
            Some("10.1000/from-field")
        );
    }

    #[test]
    fn an_invalid_doi_and_url_leave_no_doi() {
        let bib = "\
@misc{bad,
  title = {No identifier},
  year = {2017},
  doi = {not-a-doi},
  url = {https://example.org/page}
}
";
        let paired = pair_with_server(bib, &[]).unwrap();
        assert_eq!(paired.len(), 1, "{paired:?}");
        assert_eq!(paired[0].entry_id, None);
        assert_eq!(paired[0].doi, None);
        assert_eq!(paired[0].source.field("doi"), Some("not-a-doi"));
        assert_eq!(
            paired[0].source.field("url"),
            Some("https://example.org/page")
        );
    }

    #[test]
    fn unequal_leftovers_pair_by_key_then_title_and_year() {
        let bib = "\
@article{alpha,
  title = {Alpha paper},
  year = {2020}
}
@manual{beta,
  title = {Beta manual},
  year = {2021}
}
@misc{gamma,
  title = {Gamma note},
  year = {2022}
}
";
        let server = vec![
            json!({"id": "s-gamma", "metadata": {"title": "Gamma note", "year": 2022}}),
            json!({"id": "s-stale", "metadata": {"title": "Beta manual", "year": 1999}}),
            json!({"id": "s-alpha", "metadata": {"citation_key": "alpha", "title": "Changed", "year": 1990}}),
            json!({"id": "s-extra", "metadata": {"title": "Something else", "year": 2011}}),
        ];
        let paired = pair_with_server(bib, &server).unwrap();
        assert_eq!(paired.len(), 3, "{paired:?}");
        let by_key = |key: &str| paired.iter().find(|item| item.source.key == key).unwrap();
        assert_eq!(by_key("alpha").entry_id.as_deref(), Some("s-alpha"));
        assert_eq!(by_key("gamma").entry_id.as_deref(), Some("s-gamma"));
        let beta = by_key("beta");
        assert_eq!(beta.entry_id, None);
        assert_eq!(beta.doi, None);
        assert_eq!(beta.source.entry_type, "manual");
        assert_eq!(beta.source.field("title"), Some("Beta manual"));
        assert_eq!(beta.source.field("year"), Some("2021"));
    }

    #[test]
    fn unpaired_entries_keep_their_source_snapshot() {
        let bib = "\
@article{matched,
  title = {Matched title},
  year = {2020},
  doi = {10.1000/matched}
}
@misc{doi-only,
  title = {Unmatched doi},
  year = {2019},
  doi = {10.1000/unmatched},
  note = {doi snapshot}
}
@manual{no-doi,
  title = {No doi note},
  year = {2018},
  howpublished = {Web}
}
";
        let server = vec![
            json!({"id": "s-matched", "metadata": {"doi": "10.1000/matched", "title": "Matched title"}}),
        ];
        let paired = pair_with_server(bib, &server).unwrap();
        assert_eq!(paired.len(), 3, "{paired:?}");
        let by_key = |key: &str| paired.iter().find(|item| item.source.key == key).unwrap();
        assert_eq!(by_key("matched").entry_id.as_deref(), Some("s-matched"));
        assert_eq!(by_key("matched").doi.as_deref(), Some("10.1000/matched"));
        let doi_only = by_key("doi-only");
        assert_eq!(doi_only.entry_id, None);
        assert_eq!(doi_only.doi.as_deref(), Some("10.1000/unmatched"));
        assert_eq!(doi_only.source.entry_type, "misc");
        assert_eq!(doi_only.source.field("title"), Some("Unmatched doi"));
        assert_eq!(doi_only.source.field("note"), Some("doi snapshot"));
        assert_eq!(doi_only.source.field("year"), Some("2019"));
        let no_doi = by_key("no-doi");
        assert_eq!(no_doi.entry_id, None);
        assert_eq!(no_doi.doi, None);
        assert_eq!(no_doi.source.entry_type, "manual");
        assert_eq!(no_doi.source.field("title"), Some("No doi note"));
        assert_eq!(no_doi.source.field("howpublished"), Some("Web"));
        assert_eq!(no_doi.source.field("year"), Some("2018"));
    }

    #[test]
    fn doi_url_and_eprint_stay_verbatim_and_a_macro_stays_bare() {
        let bib = "\
@article{sample,
  doi = {10.1142/S021963361850016_0016},
  url = {https://example.org/a_b%20c#frag},
  eprint = {cond-mat/9801_5},
  month = jan
}
";
        let out = round_trip(bib);
        assert_eq!(out, bib, "{out}");
        assert!(!out.contains("\\_"), "{out}");
        assert!(!out.contains("\\%"), "{out}");
        assert!(!out.contains("\\#"), "{out}");
        assert!(!out.contains("month = {jan}"), "{out}");
        let escaped = crate::bib_audit::clean_doi(r"10.1142/S021963361850016\_0016").unwrap();
        assert_eq!(escaped, "10.1142/s021963361850016_0016");
        let mixed =
            crate::bib_audit::clean_doi(r"https://doi.org/10.1000/abc\%20def\#frag").unwrap();
        assert_eq!(mixed, "10.1000/abc%20def#frag");
    }

    #[test]
    fn common_collisions_keep_the_letters_a_b_and_c() {
        let mut used = HashSet::new();
        let keys: Vec<_> = (0..4)
            .map(|_| dedup_key("voter".into(), &mut used).unwrap())
            .collect();
        assert_prefix(&keys, &["voter", "votera", "voterb", "voterc"]);

        let mut used = HashSet::from(["voter".into(), "votera".into()]);
        assert_eq!(dedup_key("voter".into(), &mut used).unwrap(), "voterb");
        assert_eq!(dedup_key("voter".into(), &mut used).unwrap(), "voterc");
    }

    #[test]
    fn collisions_continue_past_z_with_aa() {
        let mut used = HashSet::new();
        let keys: Vec<_> = (0..28)
            .map(|_| dedup_key("voter".into(), &mut used).unwrap())
            .collect();
        assert_eq!(keys[26], "voterz");
        assert_eq!(keys[27], "voteraa");
        assert!(keys.iter().all(|key| key.is_ascii()));
    }

    #[test]
    fn prop_keys_stay_unique_through_the_suffix_cap() {
        finish_within(std::time::Duration::from_secs(10), || {
            let mut saturated = HashSet::from(["dup".into()]);
            for byte in b'a'..=255 {
                saturated.insert(format!("dup{}", char::from(byte)));
            }
            for count in (0..=128).chain([159, 160, 161, 255, 256, 257, 675, 676, 701, 702]) {
                assert_unique_collisions(count, HashSet::new());
                assert_unique_collisions(count, HashSet::from(["dup".into()]));
                if count <= 676 && matches!(count, 0 | 1 | 3 | 26 | 27 | 160 | 256 | 675 | 676) {
                    assert_unique_collisions(count, saturated.clone());
                }
            }
            assert_unique_collisions(1 + 26 + 26 * 26, HashSet::new());
        });
    }

    #[test]
    fn suffixes_stop_at_zz_on_import_and_export() {
        // One base key, `a`–`z`, then `aa`–`zz`.
        const KEYS: usize = 1 + 26 + 26 * 26;
        let (import_keys, export_keys) = finish_within(std::time::Duration::from_secs(10), || {
            let mut bib = String::new();
            for index in 0..KEYS {
                bib.push_str(&format!(
                    "@article{{same,\n  title = {{Paper {index}}}\n}}\n\n"
                ));
            }
            let server: Vec<Value> = (0..KEYS)
                .map(|index| json!({"id": format!("e{index}")}))
                .collect();
            let paired = pair_with_server(&bib, &server).expect("import");
            let import_keys: Vec<String> =
                paired.iter().map(|item| item.source.key.clone()).collect();
            let colliding: Vec<BibSource> = (0..KEYS)
                .map(|index| BibSource {
                    key: "same".into(),
                    entry_type: "article".into(),
                    fields: vec![("title".into(), format!("Paper {index}"))],
                })
                .collect();
            let exported = render_bibliography(&colliding).expect("export");
            let export_keys: Vec<String> = parse_bibtex_raw(&exported)
                .expect("export")
                .into_iter()
                .map(|entry| entry.key)
                .collect();
            (import_keys, export_keys)
        });
        assert_eq!(import_keys.len(), KEYS);
        assert_eq!(export_keys.len(), KEYS);
        assert_unique(&import_keys);
        assert_unique(&export_keys);
        assert_prefix(&import_keys, &["same", "samea", "sameb", "samec"]);
        assert_prefix(&export_keys, &["same", "samea", "sameb", "samec"]);
        assert_eq!(import_keys[26], "samez");
        assert_eq!(import_keys[27], "sameaa");
        assert_eq!(import_keys[KEYS - 1], "samezz");
        assert_eq!(export_keys[26], "samez");
        assert_eq!(export_keys[27], "sameaa");
        assert_eq!(export_keys[KEYS - 1], "samezz");

        let mut bib = String::new();
        for index in 0..=KEYS {
            bib.push_str(&format!(
                "@article{{same,\n  title = {{Paper {index}}}\n}}\n\n"
            ));
        }
        let server: Vec<Value> = (0..=KEYS)
            .map(|index| json!({"id": format!("e{index}")}))
            .collect();
        let import_error = pair_with_server(&bib, &server).unwrap_err();
        let BibliographyError::CitationKey(import_error) = import_error else {
            panic!("import returned {import_error}");
        };
        assert_eq!(import_error.base, "same");
        assert_eq!(
            import_error.to_string(),
            "no free citation key for \"same\" after suffixes a-z and aa-zz"
        );
        let wrapped = BibliographyError::from(import_error);
        assert_eq!(
            wrapped.to_string(),
            "no free citation key for \"same\" after suffixes a-z and aa-zz"
        );
        assert_eq!(
            std::error::Error::source(&wrapped)
                .map(ToString::to_string)
                .as_deref(),
            Some("no free citation key for \"same\" after suffixes a-z and aa-zz")
        );
        let parse_error = BibliographyError::Parse("bad bib".into());
        assert_eq!(parse_error.to_string(), "bad bib");
        assert!(std::error::Error::source(&parse_error).is_none());

        let colliding: Vec<BibSource> = (0..=KEYS)
            .map(|index| BibSource {
                key: "same".into(),
                entry_type: "article".into(),
                fields: vec![("title".into(), format!("Paper {index}"))],
            })
            .collect();
        let export_error = render_bibliography(&colliding).unwrap_err();
        assert_eq!(export_error.base, "same");
        assert_eq!(
            export_error.to_string(),
            "no free citation key for \"same\" after suffixes a-z and aa-zz"
        );
    }

    fn assert_unique_collisions(count: usize, mut used: HashSet<String>) {
        let blocked = used.clone();
        let only_base = blocked.len() == 1 && blocked.contains("dup");
        let byte_tails_are_full = blocked.contains("dup")
            && blocked.contains("dupa")
            && blocked.contains("dupz")
            && blocked.contains(&format!("dup{}", char::from(255)));
        let keys: Vec<String> = (0..count)
            .map(|_| dedup_key("dup".into(), &mut used).unwrap())
            .collect();
        assert_eq!(keys.len(), count);
        assert_unique(&keys);
        assert!(
            keys.iter().all(|key| !blocked.contains(key)),
            "count {count}"
        );
        assert!(keys.iter().all(|key| key.is_ascii()), "count {count}");
        if blocked.is_empty() && count >= 4 {
            assert_prefix(&keys, &["dup", "dupa", "dupb", "dupc"]);
        }
        if only_base && count >= 3 {
            assert_prefix(&keys, &["dupa", "dupb", "dupc"]);
        }
        if byte_tails_are_full && count >= 2 {
            assert_prefix(&keys, &["dupaa", "dupab"]);
        }
    }

    fn assert_prefix(keys: &[String], expected: &[&str]) {
        assert!(keys.len() >= expected.len());
        for (key, want) in keys.iter().zip(expected) {
            assert_eq!(key, want);
        }
    }

    fn assert_unique(keys: &[String]) {
        let unique: HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }

    fn finish_within<T>(limit: std::time::Duration, work: impl FnOnce() -> T + Send + 'static) -> T
    where
        T: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            let _ = tx.send(result);
        });
        match rx.recv_timeout(limit) {
            Ok(Ok(value)) => value,
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eprintln!("citation key disambiguation did not finish within {limit:?}");
                std::process::exit(101);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("citation key disambiguation stopped before returning a key");
            }
        }
    }
}
