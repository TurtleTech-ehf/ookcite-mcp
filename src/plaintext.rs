//! Plaintext bibliography split and local BibTeX rendering.
//!
//! Collection import on the API accepts only BibTeX/RIS. A pasted citation
//! list is split here (or via `/parse-citations`), resolved, then rendered
//! as `.bib` without requiring a collection or API key for small batches.

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

pub fn render_bibtex_entries(entries: &[serde_json::Value]) -> String {
    crate::bib_export::render_stored_entries(entries)
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
        assert!(bib.contains("@article{maimanStimulatedOpticalRadiation1960,"));
        assert!(bib.contains("title = {Stimulated {Optical} {Radiation} in {Ruby}}"));
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
