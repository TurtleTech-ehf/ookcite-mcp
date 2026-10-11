//! Local per-collection metadata overrides.
//!
//! The OokCite API accepts `PUT .../metadata` but does not yet persist every
//! field this client sends (see `docs/ookcite-api-export.md`). Each edit made
//! through `update_entry_metadata` is therefore also written to a JSON file on
//! this machine, and `export_collection` lays it over the server record before
//! the BibTeX is rendered. The file is the caller's record of intent: a field
//! set to a string replaces the provider value, and a field set to `null` is
//! removed from the export.
//!
//! Location: `$OOKCITE_OVERRIDES_DIR/<collection>.json` when that variable is
//! set, else `$XDG_DATA_HOME/ookcite/overrides/<collection>.json`, else
//! `~/.local/share/ookcite/overrides/<collection>.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::collection_entries::person_from_name;
use crate::tool_args::UpdateEntryMetadataArgs;

/// One entry's overrides. `None` in `fields` removes that field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryOverride {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, Option<String>>,
    /// Replacement author list as stored person objects. An empty list removes authors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<Value>>,
    /// Replacement editor list. An empty list removes editors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
}

impl EntryOverride {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
            && self.authors.is_none()
            && self.editor.is_none()
            && self.year.is_none()
    }

    /// Fold one `update_entry_metadata` call into this override.
    pub fn record(&mut self, args: &UpdateEntryMetadataArgs) {
        if let Some(names) = &args.clear {
            for name in names {
                let key = name.trim().to_ascii_lowercase();
                match key.as_str() {
                    "" => {}
                    "authors" | "author" => self.authors = Some(Vec::new()),
                    "editor" | "editors" => self.editor = Some(Vec::new()),
                    "year" => self.year = None,
                    _ => {
                        self.fields.insert(key, None);
                    }
                }
            }
        }
        let pairs: [(&str, &Option<String>); 18] = [
            ("title", &args.title),
            ("journal", &args.journal),
            ("volume", &args.volume),
            ("issue", &args.issue),
            ("pages", &args.pages),
            ("publisher", &args.publisher),
            ("doi", &args.doi),
            ("url", &args.url),
            ("version", &args.version),
            ("eid", &args.eid),
            ("booktitle", &args.booktitle),
            ("entry_type", &args.entry_type),
            ("institution", &args.institution),
            ("chapter", &args.chapter),
            ("number", &args.number),
            ("citation_key", &args.citation_key),
            ("howpublished", &args.howpublished),
            ("note", &args.note),
        ];
        for (key, value) in pairs {
            if let Some(value) = value {
                let value = value.trim();
                let stored = (!value.is_empty()).then(|| value.to_string());
                self.fields.insert(key.to_string(), stored);
            }
        }
        if let Some(names) = &args.authors {
            self.authors = Some(names_to_people(names));
        }
        if let Some(names) = &args.editor {
            self.editor = Some(names_to_people(names));
        }
        if let Some(year) = args.year {
            self.year = Some(year);
        }
    }
}

fn names_to_people(names: &[String]) -> Vec<Value> {
    names
        .iter()
        .filter(|name| !name.trim().is_empty())
        .map(|name| person_from_name(name))
        .collect()
}

/// Overrides of one collection, keyed by `doi:<lowercase doi>` or `id:<entry id>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionOverrides {
    #[serde(default = "format_version")]
    pub version: u32,
    #[serde(default)]
    pub collection: String,
    #[serde(default)]
    pub entries: BTreeMap<String, EntryOverride>,
}

fn format_version() -> u32 {
    1
}

impl CollectionOverrides {
    pub fn new(collection: &str) -> Self {
        Self {
            version: 1,
            collection: collection.to_string(),
            entries: BTreeMap::new(),
        }
    }

    /// The override for an entry, matched by DOI first and then by entry id.
    pub fn for_entry(&self, entry_id: Option<&str>, doi: Option<&str>) -> Option<&EntryOverride> {
        doi.and_then(|doi| self.entries.get(&doi_key(doi)))
            .or_else(|| entry_id.and_then(|id| self.entries.get(&id_key(id))))
    }

    /// Store key for an entry: its DOI when it has one, else its entry id.
    pub fn key_for(entry_id: &str, doi: Option<&str>) -> String {
        match doi.map(str::trim).filter(|doi| !doi.is_empty()) {
            Some(doi) => doi_key(doi),
            None => id_key(entry_id),
        }
    }

    pub fn record(&mut self, key: String, args: &UpdateEntryMetadataArgs) {
        let entry = self.entries.entry(key.clone()).or_default();
        entry.record(args);
        if entry.is_empty() {
            self.entries.remove(&key);
        }
    }

    /// Lay every override over a collection body's entries, in place.
    pub fn apply_to_entries(&self, entries: &mut [Value]) {
        for entry in entries.iter_mut() {
            let entry_id = entry.get("id").and_then(Value::as_str).map(str::to_string);
            let meta = entry
                .get("metadata")
                .cloned()
                .unwrap_or_else(|| entry.clone());
            let doi = meta.get("doi").and_then(Value::as_str).map(str::to_string);
            let Some(ov) = self.for_entry(entry_id.as_deref(), doi.as_deref()) else {
                continue;
            };
            let merged = apply(&meta, ov);
            if entry.get("metadata").is_some() {
                entry["metadata"] = merged;
            } else {
                *entry = merged;
            }
        }
    }
}

pub fn doi_key(doi: &str) -> String {
    let doi = doi.trim();
    let doi = doi
        .strip_prefix("doi:")
        .or_else(|| doi.strip_prefix("https://doi.org/"))
        .unwrap_or(doi);
    format!("doi:{}", doi.to_ascii_lowercase())
}

fn id_key(id: &str) -> String {
    format!("id:{}", id.trim())
}

/// Stored metadata with one override laid over it. Every touched field is
/// listed in `user_fields`, so export prefers it over the provider record,
/// and a removed field stays removed instead of falling back to the provider.
pub fn apply(meta: &Value, ov: &EntryOverride) -> Value {
    let mut out = meta.as_object().cloned().unwrap_or_default();
    let mut locked: Vec<String> = Vec::new();
    for (key, value) in &ov.fields {
        match value {
            Some(value) => {
                out.insert(key.clone(), Value::String(value.clone()));
            }
            None => {
                out.remove(key);
            }
        }
        locked.push(key.clone());
    }
    if let Some(people) = &ov.authors {
        if people.is_empty() {
            out.remove("authors");
        } else {
            out.insert("authors".into(), Value::Array(people.clone()));
        }
        locked.push("authors".into());
    }
    if let Some(people) = &ov.editor {
        if people.is_empty() {
            out.remove("editor");
        } else {
            out.insert("editor".into(), Value::Array(people.clone()));
        }
        locked.push("editor".into());
    }
    if let Some(year) = ov.year {
        let mut date = out
            .get("date")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        date.insert("year".into(), serde_json::json!(year));
        out.insert("date".into(), Value::Object(date));
        locked.push("year".into());
    }
    let mut fields = out
        .get("user_fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for key in locked {
        if !fields.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(&key))
        }) {
            fields.push(Value::String(key));
        }
    }
    out.insert("user_fields".into(), Value::Array(fields));
    Value::Object(out)
}

/// Directory holding the override files.
pub fn store_dir() -> Option<PathBuf> {
    if cfg!(test) {
        // Unit tests never touch the real data directory.
        return env_path("OOKCITE_OVERRIDES_DIR");
    }
    if let Some(dir) = env_path("OOKCITE_OVERRIDES_DIR") {
        return Some(dir);
    }
    if let Some(data) = env_path("XDG_DATA_HOME") {
        return Some(data.join("ookcite").join("overrides"));
    }
    env_path("HOME").map(|home| {
        home.join(".local")
            .join("share")
            .join("ookcite")
            .join("overrides")
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// File name for a collection: the name with anything outside `[A-Za-z0-9._-]` replaced by `_`.
pub fn file_name(collection: &str) -> String {
    let safe: String = collection
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_start_matches('.');
    if safe.is_empty() {
        "collection.json".into()
    } else {
        format!("{safe}.json")
    }
}

pub fn path_in(dir: &Path, collection: &str) -> PathBuf {
    dir.join(file_name(collection))
}

pub fn load_from(dir: &Path, collection: &str) -> Result<CollectionOverrides, String> {
    let path = path_in(dir, collection);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("Override file {} is not valid: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CollectionOverrides::new(collection))
        }
        Err(error) => Err(format!("Cannot read {}: {error}", path.display())),
    }
}

/// Write the file through a temporary sibling so a crash never leaves half a file.
pub fn save_to(dir: &Path, overrides: &CollectionOverrides) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("Cannot create {}: {error}", dir.display()))?;
    let path = path_in(dir, &overrides.collection);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(overrides)
        .map_err(|error| format!("Cannot encode overrides: {error}"))?;
    std::fs::write(&tmp, text + "\n")
        .map_err(|error| format!("Cannot write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
    Ok(path)
}

pub fn load(collection: &str) -> Result<CollectionOverrides, String> {
    match store_dir() {
        Some(dir) => load_from(&dir, collection),
        None => Ok(CollectionOverrides::new(collection)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(collection: &str, entry: &str) -> UpdateEntryMetadataArgs {
        UpdateEntryMetadataArgs {
            collection: collection.into(),
            entry_id: entry.into(),
            ..Default::default()
        }
    }

    #[test]
    fn record_keeps_values_and_removals() {
        let mut store = CollectionOverrides::new("paper");
        let mut edit = args("paper", "doi:10.1/x");
        edit.title = Some("{PACKMOL}: A package".into());
        edit.citation_key = Some("martinezPackmol2009".into());
        edit.url = Some(String::new());
        edit.clear = Some(vec!["editor".into(), "Journal".into()]);
        edit.authors = Some(vec!["de Buyl, Pierre".into(), "{FORCE11 Group}".into()]);
        store.record(CollectionOverrides::key_for("e1", Some("10.1/X")), &edit);
        let entry = store
            .entries
            .get("doi:10.1/x")
            .expect("keyed by lowercase DOI");
        assert_eq!(
            entry.fields["title"].as_deref(),
            Some("{PACKMOL}: A package")
        );
        assert_eq!(entry.fields["url"], None);
        assert_eq!(entry.fields["journal"], None);
        assert_eq!(entry.editor, Some(Vec::new()));
        let authors = entry.authors.as_ref().unwrap();
        assert_eq!(authors[0], json!({"family": "de Buyl", "given": "Pierre"}));
        assert_eq!(
            authors[1],
            json!({"family": "FORCE11 Group", "literal": true})
        );

        let mut later = args("paper", "doi:10.1/x");
        later.url = Some("https://example.org".into());
        store.record("doi:10.1/x".into(), &later);
        let entry = &store.entries["doi:10.1/x"];
        assert_eq!(entry.fields["url"].as_deref(), Some("https://example.org"));
        assert_eq!(
            entry.fields["citation_key"].as_deref(),
            Some("martinezPackmol2009")
        );
    }

    #[test]
    fn apply_sets_removes_and_locks() {
        let meta = json!({
            "title": "P<scp>ACKMOL</scp>",
            "url": "https://landing.example/very/long",
            "editor": [{"family": "Bray", "given": "T."}],
            "journal": "Zenodo",
            "user_fields": ["authors"],
        });
        let mut ov = EntryOverride::default();
        ov.fields.insert("title".into(), Some("{PACKMOL}".into()));
        ov.fields.insert("url".into(), None);
        ov.fields.insert("journal".into(), None);
        ov.editor = Some(Vec::new());
        ov.year = Some(2010);
        let out = apply(&meta, &ov);
        assert_eq!(out["title"], "{PACKMOL}");
        assert!(out.get("url").is_none());
        assert!(out.get("journal").is_none());
        assert!(out.get("editor").is_none());
        assert_eq!(out["date"]["year"], 2010);
        let locked: Vec<&str> = out["user_fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for key in ["authors", "title", "url", "journal", "editor", "year"] {
            assert!(locked.contains(&key), "{key} not locked: {locked:?}");
        }
    }

    #[test]
    fn apply_to_entries_matches_by_doi_then_id() {
        let mut store = CollectionOverrides::new("paper");
        let mut by_doi = args("paper", "x");
        by_doi.citation_key = Some("keyA".into());
        store.record(CollectionOverrides::key_for("e1", Some("10.1/A")), &by_doi);
        let mut by_id = args("paper", "x");
        by_id.citation_key = Some("keyB".into());
        store.record(CollectionOverrides::key_for("e2", None), &by_id);
        let mut entries = vec![
            json!({"id": "e1", "metadata": {"doi": "10.1/a", "title": "A"}}),
            json!({"id": "e2", "metadata": {"title": "B"}}),
            json!({"id": "e3", "metadata": {"doi": "10.1/c", "title": "C"}}),
        ];
        store.apply_to_entries(&mut entries);
        assert_eq!(entries[0]["metadata"]["citation_key"], "keyA");
        assert_eq!(entries[1]["metadata"]["citation_key"], "keyB");
        assert!(entries[2]["metadata"].get("citation_key").is_none());
    }

    #[test]
    fn store_round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CollectionOverrides::new("readcon cpc/v3");
        let mut edit = args("readcon cpc/v3", "doi:10.1/x");
        edit.entry_type = Some("techreport".into());
        store.record("doi:10.1/x".into(), &edit);
        let path = save_to(dir.path(), &store).unwrap();
        assert_eq!(path.file_name().unwrap(), "readcon_cpc_v3.json");
        let back = load_from(dir.path(), "readcon cpc/v3").unwrap();
        assert_eq!(back, store);
        let missing = load_from(dir.path(), "other").unwrap();
        assert!(missing.entries.is_empty());
    }
}
