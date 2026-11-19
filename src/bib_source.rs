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
//! - `#` concatenation is joined, and an `@string` macro is not expanded;
//! - a Unicode en dash or em dash is written `--` or `---`;
//! - a bare `&`, `%`, `#`, or `_` is escaped, and an escaped one is not
//!   escaped again.
//!
//! Braces, `--`, name particles, and LaTeX accents are copied as written.
//! A citation key is kept. A key is generated only when the source entry
//! has none. A repeated key keeps the first spelling and then takes the
//! suffixes `a`, `b`, `c`, through `z`, `aa`, `ab`, so a long run of
//! collisions still ends. Resolver fields are not added unless export is
//! asked to enrich, and then only a field the source left empty.

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
            .map(|(_, value)| value.as_str())
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
/// A DOI matches first. Entries left over match in order when the two sides
/// have the same number of leftovers, which is how a server that dropped the
/// DOI of a web page is still tied to the source entry.
pub fn pair_with_server(content: &str, server: &[Value]) -> Result<Vec<PairedSource>, String> {
    let entries = parse_bibtex_raw(content)?;
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
    let source_left: Vec<usize> = paired
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| slot.is_none().then_some(index))
        .collect();
    let server_left: Vec<usize> = used
        .iter()
        .enumerate()
        .filter_map(|(index, taken)| (!taken).then_some(index))
        .collect();
    if source_left.len() == server_left.len() {
        for (source_index, server_index) in source_left.into_iter().zip(server_left) {
            paired[source_index] = Some(server_index);
        }
    }
    let mut seen_keys = HashSet::new();
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(server_index) = paired[index] else {
            continue;
        };
        let source = to_source(entry, &mut seen_keys);
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

#[cfg(test)]
pub fn render_bibliography(entries: &[BibSource]) -> String {
    let mut used = HashSet::new();
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&render_entry(entry, &mut used));
    }
    out
}

pub fn render_entry(entry: &BibSource, used: &mut HashSet<String>) -> String {
    let mut key = entry.key.trim().to_string();
    if key.is_empty() {
        key = generate_key(entry);
    }
    if key.is_empty() {
        key = "ref".into();
    }
    key = dedup_key(key, used);
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
        let value = prepare_field_value(value);
        if value.trim().is_empty() {
            continue;
        }
        lines.push(format!("  {name} = {{{value}}}"));
    }
    format!("@{entry_type}{{{key},\n{}\n}}\n", lines.join(",\n"))
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
#[cfg(test)]
///
/// Entries are aligned by DOI, then by a brace-stripped title, then by
/// position. `entries_differing` counts an entry whose type or fields differ.
/// A key change alone does not. Values are compared after collapsing
/// whitespace. The source value is escaped and its Unicode dashes are turned
/// into TeX dashes first, which is the export's only value change.
pub fn diff_bibliographies(source: &str, exported: &str) -> Result<BibliographyDiff, String> {
    compare_bibliographies(source, exported, true)
}

/// The same alignment as [`diff_bibliographies`], comparing values after
#[cfg(test)]
/// whitespace is collapsed and nothing else. This is the count used for the
/// lossy export that the round trip replaces.
pub fn diff_report(source: &str, exported: &str) -> Result<BibliographyDiff, String> {
    compare_bibliographies(source, exported, false)
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
                Some(exported) if !values_equal(value, exported, canonical) => {
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
fn values_equal(source: &str, exported: &str, canonical: bool) -> bool {
    let source = collapse_whitespace(source);
    let exported = collapse_whitespace(exported);
    if canonical {
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

fn to_source(entry: &RawBibEntry, used: &mut HashSet<String>) -> BibSource {
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
    key = dedup_key(key, used);
    BibSource {
        key,
        entry_type: entry.entry_type.clone(),
        fields: entry.fields.clone(),
    }
}

fn source_doi(entry: &RawBibEntry) -> Option<String> {
    let raw = field_raw(entry, "doi").or_else(|| field_raw(entry, "url"))?;
    crate::bib_audit::clean_doi(raw)
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

/// Return `key` when it is free. Otherwise append `a`, `b`, `c`, … `z`, `aa`, `ab`, …
///
/// The first twenty-six collisions are the single letters this exporter already
/// wrote. The tail is bijective base 26, so the search does not stall once a
/// one-byte suffix has been used up.
pub(crate) fn dedup_key(key: String, used: &mut HashSet<String>) -> String {
    if used.insert(key.clone()) {
        return key;
    }
    let base = key;
    // Least-significant digit first, 0 = 'a'. Each step is a new suffix.
    let mut digits = Vec::new();
    loop {
        next_letter_suffix(&mut digits);
        let suffix = format_letter_suffix(&digits);
        let candidate = format!("{base}{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
}

fn next_letter_suffix(digits: &mut Vec<u8>) {
    for digit in digits.iter_mut() {
        if *digit < 25 {
            *digit += 1;
            return;
        }
        *digit = 0;
    }
    digits.push(0);
}

fn format_letter_suffix(digits: &[u8]) -> String {
    digits
        .iter()
        .rev()
        .map(|digit| char::from(b'a' + *digit))
        .collect()
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
            .collect();
        render_bibliography(&sources)
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
    fn pairing_uses_the_doi_then_the_leftover_order() {
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
        assert_eq!(by_key("web").entry_id.as_deref(), Some("e2"));
        assert_eq!(by_key("web").source.entry_type, "manual");
    }

    #[test]
    fn common_collisions_keep_the_letters_a_b_and_c() {
        let mut used = HashSet::new();
        let keys: Vec<_> = (0..4)
            .map(|_| dedup_key("voter".into(), &mut used))
            .collect();
        assert_prefix(&keys, &["voter", "votera", "voterb", "voterc"]);

        let mut used = HashSet::from(["voter".into(), "votera".into()]);
        assert_eq!(dedup_key("voter".into(), &mut used), "voterb");
        assert_eq!(dedup_key("voter".into(), &mut used), "voterc");
    }

    #[test]
    fn collisions_continue_past_z_with_aa() {
        let mut used = HashSet::new();
        let keys: Vec<_> = (0..28)
            .map(|_| dedup_key("voter".into(), &mut used))
            .collect();
        assert_eq!(keys[26], "voterz");
        assert_eq!(keys[27], "voteraa");
        assert!(keys.iter().all(|key| key.is_ascii()));
    }

    #[test]
    fn prop_keys_stay_unique_for_any_collision_count() {
        finish_within(std::time::Duration::from_secs(10), || {
            let mut saturated = HashSet::from(["dup".into()]);
            for byte in b'a'..=255 {
                saturated.insert(format!("dup{}", char::from(byte)));
            }
            for count in (0..=128).chain([159, 160, 161, 255, 256, 257, 1000, 1001, 2048]) {
                assert_unique_collisions(count, HashSet::new());
                assert_unique_collisions(count, HashSet::from(["dup".into()]));
                if matches!(count, 0 | 1 | 3 | 26 | 27 | 160 | 256 | 1000 | 2048) {
                    assert_unique_collisions(count, saturated.clone());
                }
            }
        });
    }

    #[test]
    fn a_thousand_colliding_keys_finish_on_import_and_export() {
        const COUNT: usize = 1000;
        let (import_keys, export_keys) = finish_within(std::time::Duration::from_secs(10), || {
            let mut bib = String::new();
            for index in 0..COUNT {
                bib.push_str(&format!(
                    "@article{{same,\n  title = {{Paper {index}}}\n}}\n\n"
                ));
            }
            let server: Vec<Value> = (0..COUNT)
                .map(|index| json!({"id": format!("e{index}")}))
                .collect();
            let paired = pair_with_server(&bib, &server).expect("import");
            let import_keys: Vec<String> =
                paired.iter().map(|item| item.source.key.clone()).collect();
            let colliding: Vec<BibSource> = (0..COUNT)
                .map(|index| BibSource {
                    key: "same".into(),
                    entry_type: "article".into(),
                    fields: vec![("title".into(), format!("Paper {index}"))],
                })
                .collect();
            let exported = render_bibliography(&colliding);
            let export_keys: Vec<String> = parse_bibtex_raw(&exported)
                .expect("export")
                .into_iter()
                .map(|entry| entry.key)
                .collect();
            (import_keys, export_keys)
        });
        assert_eq!(import_keys.len(), COUNT);
        assert_eq!(export_keys.len(), COUNT);
        assert_unique(&import_keys);
        assert_unique(&export_keys);
        assert_prefix(&import_keys, &["same", "samea", "sameb", "samec"]);
        assert_prefix(&export_keys, &["same", "samea", "sameb", "samec"]);
        assert_eq!(import_keys[26], "samez");
        assert_eq!(import_keys[27], "sameaa");
        assert_eq!(export_keys[26], "samez");
        assert_eq!(export_keys[27], "sameaa");
        assert!(import_keys.iter().all(|key| key.is_ascii()));
        assert!(export_keys.iter().all(|key| key.is_ascii()));
    }

    #[test]
    fn readcon_and_amsel_round_trips_are_unchanged() {
        let paths = [
            "/cursor/stores/bot/ttkit/readcon-ookcite-repro/rg_main.bib",
            "/cursor/stores/bot/ttkit/amsel-ookcite-repro/amsel-refs.bib",
        ];
        for path in paths {
            if !std::path::Path::new(path).exists() {
                continue;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let exported = round_trip(&source);
            let diff = diff_bibliographies(&source, &exported).unwrap();
            assert_eq!(diff.key_changes, 0, "{path} {diff:?}");
            assert_eq!(diff.type_changes, 0, "{path} {diff:?}");
            assert_eq!(diff.dropped_fields, 0, "{path} {diff:?}");
            assert_eq!(diff.added_fields, 0, "{path} {diff:?}");
            assert_eq!(diff.value_changes, 0, "{path} {diff:?}");
            assert_eq!(diff.entries_differing, 0, "{path} {diff:?}");
            assert_eq!(diff.export_entries, diff.source_entries, "{path} {diff:?}");
            let source_keys: Vec<_> = parse_bibtex_raw(&source)
                .unwrap()
                .into_iter()
                .map(|entry| entry.key)
                .collect();
            let export_keys: Vec<_> = parse_bibtex_raw(&exported)
                .unwrap()
                .into_iter()
                .map(|entry| entry.key)
                .collect();
            assert_eq!(export_keys, source_keys, "{path}");
        }
    }

    fn assert_unique_collisions(count: usize, mut used: HashSet<String>) {
        let blocked = used.clone();
        let only_base = blocked.len() == 1 && blocked.contains("dup");
        let byte_tails_are_full = blocked.contains("dup")
            && blocked.contains("dupa")
            && blocked.contains("dupz")
            && blocked.contains(&format!("dup{}", char::from(255)));
        let keys: Vec<String> = (0..count)
            .map(|_| dedup_key("dup".into(), &mut used))
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
