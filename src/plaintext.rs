//! Plaintext bibliography split and local BibTeX rendering.
//!
//! Collection import on the API accepts only BibTeX/RIS. A pasted citation
//! list is split here (or via `/parse-citations`), resolved, then rendered
//! as `.bib` without requiring a collection or API key for small batches.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BibliographyKind {
    Bibtex,
    Ris,
    Plaintext,
}

pub fn detect_bibliography_kind(content: &str, format_hint: &str) -> BibliographyKind {
    match format_hint.trim().to_ascii_lowercase().as_str() {
        "bibtex" | "bib" | ".bib" => BibliographyKind::Bibtex,
        "ris" | ".ris" => BibliographyKind::Ris,
        "plaintext" | "plain" | "text" | "txt" => BibliographyKind::Plaintext,
        _ => {
            if looks_like_ris(content) {
                BibliographyKind::Ris
            } else if looks_like_bibtex(content) {
                BibliographyKind::Bibtex
            } else {
                BibliographyKind::Plaintext
            }
        }
    }
}

pub fn looks_like_bibtex(content: &str) -> bool {
    content.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with('@') && t.contains('{')
    })
}

pub fn looks_like_ris(content: &str) -> bool {
    content.lines().any(|line| {
        let t = line.trim_start();
        t.len() >= 5 && t[..2].eq_ignore_ascii_case("TY") && t[2..].trim_start().starts_with('-')
    })
}

pub fn looks_like_citation_start(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('[') {
        return t.find(']').is_some_and(|end| end > 1);
    }
    if t.starts_with('(') {
        if let Some(end) = t.find(')') {
            return end > 1 && t[1..end].chars().all(|c| c.is_ascii_digit());
        }
        return false;
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let rest = &t[digits..];
    rest.starts_with(". ")
        || rest.starts_with(") ")
        || rest.starts_with('.')
        || rest.starts_with(')')
}

pub fn strip_citation_marker(line: &str) -> &str {
    let t = line.trim();
    if t.starts_with('[') {
        if let Some(end) = t.find(']') {
            return t[end + 1..].trim();
        }
    }
    if t.starts_with('(') {
        if let Some(end) = t.find(')') {
            if end > 1 && t[1..end].chars().all(|c| c.is_ascii_digit()) {
                return t[end + 1..].trim();
            }
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &t[digits..];
        if let Some(stripped) = rest.strip_prefix(". ") {
            return stripped.trim();
        }
        if let Some(stripped) = rest.strip_prefix(") ") {
            return stripped.trim();
        }
        if let Some(stripped) = rest.strip_prefix('.') {
            return stripped.trim();
        }
        if let Some(stripped) = rest.strip_prefix(')') {
            return stripped.trim();
        }
    }
    t
}

/// Split a pasted list into citation units: numbered markers, blank lines,
/// or one unit per non-empty line.
pub fn split_plaintext_citations(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let raw_lines: Vec<&str> = normalized.lines().map(str::trim).collect();
    let non_empty: Vec<&str> = raw_lines
        .iter()
        .copied()
        .filter(|line| !line.is_empty())
        .collect();
    if non_empty.is_empty() {
        return Vec::new();
    }

    let has_markers = non_empty.iter().any(|line| looks_like_citation_start(line));
    let has_blank = normalized.contains("\n\n");
    if !has_markers && !has_blank {
        return non_empty.into_iter().map(str::to_string).collect();
    }

    let mut units = Vec::new();
    let mut current = String::new();
    let push_current = |units: &mut Vec<String>, current: &mut String| {
        let source = current.trim().to_string();
        if !source.is_empty() {
            units.push(source);
        }
        current.clear();
    };

    for line in raw_lines {
        if line.is_empty() {
            push_current(&mut units, &mut current);
            continue;
        }
        if !current.is_empty() && looks_like_citation_start(line) {
            push_current(&mut units, &mut current);
        }
        let piece = if current.is_empty() {
            strip_citation_marker(line)
        } else {
            line
        };
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(piece);
    }
    push_current(&mut units, &mut current);
    units
}

pub fn citation_units_from_parse_payload(payload: &serde_json::Value) -> Vec<String> {
    payload
        .get("citations")
        .and_then(|value| value.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    c.get("cleaned_text")
                        .and_then(|v| v.as_str())
                        .or_else(|| c.get("source_text").and_then(|v| v.as_str()))
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn sanitize_key_part(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

pub fn bibtex_cite_key(meta: &serde_json::Value) -> String {
    let family = meta
        .get("authors")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("family"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let year = meta
        .pointer("/date/year")
        .and_then(|v| v.as_i64())
        .or_else(|| meta.get("year").and_then(|v| v.as_i64()))
        .map(|y| y.to_string())
        .unwrap_or_default();
    let mut key = format!("{}{year}", sanitize_key_part(family));
    if key.is_empty() {
        if let Some(doi) = meta.get("doi").and_then(|v| v.as_str()) {
            key = sanitize_key_part(doi);
        }
    }
    if key.is_empty() {
        "ref".into()
    } else {
        key
    }
}

fn bib_entry_type(meta: &serde_json::Value) -> &'static str {
    match meta
        .get("entry_type")
        .and_then(|v| v.as_str())
        .unwrap_or("article")
        .to_ascii_lowercase()
        .as_str()
    {
        "book" => "book",
        "inbook" => "inbook",
        "incollection" => "incollection",
        "inproceedings" | "conference" => "inproceedings",
        "thesis" | "phdthesis" => "phdthesis",
        "report" | "techreport" => "techreport",
        "webpage" | "online" => "online",
        "patent" => "patent",
        "dataset" => "dataset",
        "software" => "software",
        _ => "article",
    }
}

fn field_str(meta: &serde_json::Value, key: &str) -> Option<String> {
    meta.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn format_author_bib(meta: &serde_json::Value) -> Option<String> {
    let authors = meta.get("authors").and_then(|a| a.as_array())?;
    let names: Vec<String> = authors
        .iter()
        .filter_map(|a| {
            let family = a
                .get("family")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if family.is_empty() {
                return None;
            }
            let given = a.get("given").and_then(|v| v.as_str()).unwrap_or("").trim();
            if given.is_empty() {
                Some(family.to_string())
            } else {
                Some(format!("{family}, {given}"))
            }
        })
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(" and "))
    }
}

pub fn metadata_to_bibtex(meta: &serde_json::Value, cite_key: &str) -> String {
    let bib_type = bib_entry_type(meta);
    let key = if cite_key.is_empty() { "ref" } else { cite_key };
    let mut fields = Vec::new();
    if let Some(title) = field_str(meta, "title") {
        fields.push(format!("  title = {{{title}}}"));
    }
    if let Some(author) = format_author_bib(meta) {
        fields.push(format!("  author = {{{author}}}"));
    }
    if let Some(year) = meta
        .pointer("/date/year")
        .and_then(|v| v.as_i64())
        .or_else(|| meta.get("year").and_then(|v| v.as_i64()))
    {
        fields.push(format!("  year = {{{year}}}"));
    }
    if let Some(journal) = field_str(meta, "journal") {
        fields.push(format!("  journal = {{{journal}}}"));
    }
    if let Some(volume) = field_str(meta, "volume") {
        fields.push(format!("  volume = {{{volume}}}"));
    }
    if let Some(issue) = field_str(meta, "issue").or_else(|| field_str(meta, "number")) {
        fields.push(format!("  number = {{{issue}}}"));
    }
    if let Some(pages) = field_str(meta, "pages") {
        fields.push(format!("  pages = {{{}}}", pages.replace('-', "--")));
    }
    if let Some(publisher) = field_str(meta, "publisher") {
        fields.push(format!("  publisher = {{{publisher}}}"));
    }
    if let Some(doi) = field_str(meta, "doi") {
        fields.push(format!("  doi = {{{doi}}}"));
    }
    if let Some(url) = field_str(meta, "url") {
        fields.push(format!("  url = {{{url}}}"));
    }
    format!("@{bib_type}{{{key},\n{}\n}}\n", fields.join(",\n"))
}

pub fn render_bibtex_entries(entries: &[serde_json::Value]) -> String {
    let mut used = HashSet::new();
    let mut out = String::new();
    for entry in entries {
        let mut key = bibtex_cite_key(entry);
        if used.contains(&key) {
            let base = key.clone();
            let mut n = 2u32;
            while used.contains(&key) {
                key = format!("{base}{n}");
                n += 1;
            }
        }
        used.insert(key.clone());
        out.push_str(&metadata_to_bibtex(entry, &key));
        out.push('\n');
    }
    out
}

pub fn collection_entry_metadata(entry: &serde_json::Value) -> serde_json::Value {
    entry
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| entry.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportKind {
    Bibtex,
    Csl { style: String },
}

pub fn export_kind(format: &str, style: &str) -> ExportKind {
    match format.trim().to_ascii_lowercase().as_str() {
        "" | "bib" | "bibtex" | ".bib" => ExportKind::Bibtex,
        "csl" | "text" | "plain" => ExportKind::Csl {
            style: if style.trim().is_empty() {
                "apa".into()
            } else {
                style.trim().to_string()
            },
        },
        other => ExportKind::Csl {
            style: other.to_string(),
        },
    }
}

pub fn optional_collection_name(name: &Option<String>) -> Option<&str> {
    name.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub fn attach_original_query(original: &str, body: &str) -> String {
    format!("original: {}\n{body}", original.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detects_bibtex_ris_and_plaintext() {
        assert_eq!(
            detect_bibliography_kind("@article{a,\n  title={T}\n}", "auto"),
            BibliographyKind::Bibtex
        );
        assert_eq!(
            detect_bibliography_kind("TY  - JOUR\nTI  - Hello\nER  -", ""),
            BibliographyKind::Ris
        );
        assert_eq!(
            detect_bibliography_kind("1. Maiman, T. H. Nature 1960.\n2. Einstein 1905.", "auto"),
            BibliographyKind::Plaintext
        );
        assert_eq!(
            detect_bibliography_kind("@article{a,}", "plaintext"),
            BibliographyKind::Plaintext
        );
    }

    #[test]
    fn splits_numbered_and_blank_line_lists() {
        let numbered = "1. Maiman, T. H. Stimulated Optical Radiation in Ruby. Nature (1960).\n\
2. Einstein, A. Zur Elektrodynamik bewegter Körper (1905).";
        let units = split_plaintext_citations(numbered);
        assert_eq!(units.len(), 2, "{units:?}");
        assert!(units[0].contains("Maiman"));
        assert!(!units[0].starts_with('1'), "{}", units[0]);
        assert!(units[1].contains("Einstein"));

        let hanging =
            "[1] Maiman, T. H. Stimulated Optical Radiation\n    in Ruby. Nature (1960).\n\n\
[2] Einstein, A. Zur Elektrodynamik (1905).";
        let units = split_plaintext_citations(hanging);
        assert_eq!(units.len(), 2, "{units:?}");
        assert!(units[0].contains("in Ruby"));
    }

    #[test]
    fn renders_bibtex_with_confidence_fields() {
        let meta = serde_json::json!({
            "title": "Stimulated Optical Radiation in Ruby",
            "doi": "10.1038/187493a0",
            "journal": "Nature",
            "date": { "year": 1960 },
            "authors": [{"family": "Maiman", "given": "T. H."}]
        });
        let bib = render_bibtex_entries(&[meta]);
        assert!(bib.contains("@article{Maiman1960,"));
        assert!(bib.contains("title = {Stimulated Optical Radiation in Ruby}"));
        assert!(bib.contains("doi = {10.1038/187493a0}"));
        assert!(bib.contains("author = {Maiman, T. H.}"));
    }

    #[test]
    fn export_kind_bib_vs_csl_style() {
        assert_eq!(export_kind("bib", "apa"), ExportKind::Bibtex);
        assert_eq!(
            export_kind("csl", "ieee"),
            ExportKind::Csl {
                style: "ieee".into()
            }
        );
        assert_eq!(
            export_kind("nature", "apa"),
            ExportKind::Csl {
                style: "nature".into()
            }
        );
    }

    #[test]
    fn parse_payload_prefers_cleaned_text() {
        let payload = serde_json::json!({
            "citations": [
                {"cleaned_text": "Maiman 1960", "source_text": "1. Maiman 1960"},
                {"source_text": "Einstein 1905"}
            ]
        });
        assert_eq!(
            citation_units_from_parse_payload(&payload),
            vec!["Maiman 1960".to_string(), "Einstein 1905".to_string()]
        );
    }
}
