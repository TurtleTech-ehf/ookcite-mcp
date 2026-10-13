//! Publication BibTeX rendered in this process.
//!
//! The OokCite API's `/export.bib` drops book titles, mis-types records, and
//! reprints a DOI as a URL. Export builds the `.bib` here from Crossref or
//! DataCite fields, then from any caller override stored on the entry.

use std::collections::HashSet;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BibOptions {
    /// `true` writes `@misc` plus `howpublished` instead of `@software`.
    pub software_as_misc: bool,
    /// Where an article number goes.
    pub eid_as: EidPlacement,
}

/// elsarticle-num and other classic styles never print `eid`, so an article
/// with only an article number would show no locator at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EidPlacement {
    /// Emit `eid`.
    #[default]
    Eid,
    /// Emit the article number as `pages` when the record has no page range.
    Pages,
    /// Emit the article number in `note` when the record has no page range.
    Note,
}

impl EidPlacement {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "pages" | "page" => Self::Pages,
            "note" => Self::Note,
            _ => Self::Eid,
        }
    }
}

impl BibOptions {
    pub fn from_software_entry(value: &str) -> Self {
        let misc = matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "misc" | "howpublished"
        );
        Self {
            software_as_misc: misc,
            eid_as: EidPlacement::Eid,
        }
    }

    pub fn from_args(software_entry: &str, eid_as: &str) -> Self {
        Self {
            eid_as: EidPlacement::parse(eid_as),
            ..Self::from_software_entry(software_entry)
        }
    }
}

/// Which kind of Zenodo record a software entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ZenodoRecord {
    /// A concept DOI that resolves to every version.
    Concept,
    /// One version of a concept.
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Person {
    family: String,
    given: String,
    literal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Work {
    source_type: String,
    entry_override: Option<String>,
    title: String,
    authors: Vec<Person>,
    editors: Vec<Person>,
    year: Option<i32>,
    journal: Option<String>,
    booktitle: Option<String>,
    chapter: Option<String>,
    volume: Option<String>,
    issue: Option<String>,
    report_number: Option<String>,
    pages: Option<String>,
    eid: Option<String>,
    publisher: Option<String>,
    institution: Option<String>,
    doi: String,
    url: Option<String>,
    version: Option<String>,
    note: Option<String>,
    howpublished: Option<String>,
    citation_key: Option<String>,
    from_crossref: bool,
    zenodo: Option<ZenodoRecord>,
    note_locked: bool,
    howpublished_locked: bool,
}

impl Work {
    fn empty() -> Self {
        Self {
            source_type: String::new(),
            entry_override: None,
            title: String::new(),
            authors: Vec::new(),
            editors: Vec::new(),
            year: None,
            journal: None,
            booktitle: None,
            chapter: None,
            volume: None,
            issue: None,
            report_number: None,
            pages: None,
            eid: None,
            publisher: None,
            institution: None,
            doi: String::new(),
            url: None,
            version: None,
            note: None,
            howpublished: None,
            citation_key: None,
            from_crossref: false,
            zenodo: None,
            note_locked: false,
            howpublished_locked: false,
        }
    }
}

pub fn render_stored_entries(entries: &[Value]) -> String {
    let works = entries
        .iter()
        .map(|entry| assemble(entry, None))
        .collect::<Vec<_>>();
    render_works(&works, &BibOptions::default())
}

pub fn render_pairs(pairs: &[(Value, Option<Value>)], options: &BibOptions) -> String {
    let works = pairs
        .iter()
        .map(|(stored, provider)| assemble(stored, provider.as_ref()))
        .collect::<Vec<_>>();
    render_works(&works, options)
}

/// Stored metadata with provider fields filled in. Caller locks win.
pub fn merge_provider_metadata(stored: &Value, provider: &Value) -> Value {
    let work = assemble(stored, Some(provider));
    let mut meta = stored.as_object().cloned().unwrap_or_default();
    let put = |meta: &mut serde_json::Map<String, Value>, key: &str, value: Option<String>| {
        if let Some(value) = value.filter(|text| !text.is_empty()) {
            meta.insert(key.to_string(), Value::String(value));
        }
    };
    if !work.title.is_empty() {
        meta.insert("title".into(), Value::String(work.title));
    }
    if !work.authors.is_empty() {
        let authors = work
            .authors
            .iter()
            .map(|person| {
                if person.literal {
                    json_person_literal(&person.family)
                } else if person.given.is_empty() {
                    serde_json::json!({ "family": person.family })
                } else {
                    serde_json::json!({ "family": person.family, "given": person.given })
                }
            })
            .collect();
        meta.insert("authors".into(), Value::Array(authors));
    }
    let entry_type = work
        .entry_override
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or(work.source_type);
    if !entry_type.is_empty() {
        meta.insert("entry_type".into(), Value::String(entry_type));
    }
    put(&mut meta, "journal", work.journal);
    put(&mut meta, "booktitle", work.booktitle);
    put(&mut meta, "chapter", work.chapter);
    put(&mut meta, "volume", work.volume);
    put(&mut meta, "issue", work.issue);
    put(&mut meta, "pages", work.pages);
    put(&mut meta, "eid", work.eid);
    put(&mut meta, "publisher", work.publisher);
    put(&mut meta, "institution", work.institution);
    put(&mut meta, "number", work.report_number);
    put(&mut meta, "version", work.version);
    put(&mut meta, "note", work.note);
    put(&mut meta, "howpublished", work.howpublished);
    put(&mut meta, "url", work.url);
    if !work.doi.is_empty() {
        meta.insert("doi".into(), Value::String(work.doi));
    }
    if let Some(year) = work.year {
        let mut date = meta
            .get("date")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        date.insert("year".into(), serde_json::json!(year));
        meta.insert("date".into(), Value::Object(date));
    }
    Value::Object(meta)
}

fn json_person_literal(name: &str) -> Value {
    serde_json::json!({ "family": name, "literal": true })
}

/// One collection entry plus an optional provider payload
/// (`{"crossref": ..., "datacite": ..., "zenodo": ...}`).
fn assemble(stored: &Value, provider: Option<&Value>) -> Work {
    let stored_work = work_from_stored(stored);
    let Some(record) = provider else {
        return stored_work;
    };
    let Some(mut work) = work_from_provider_record(record) else {
        return stored_work;
    };
    let locked = user_fields(stored);
    if locked.contains("title") {
        work.title = stored_work.title.clone();
    }
    let provider_has_more_authors = work.from_crossref
        && work.authors.len() > stored_work.authors.len()
        && authors_cover(&stored_work.authors, &work.authors);
    if locked.contains("authors") || (!stored_work.authors.is_empty() && !provider_has_more_authors)
    {
        work.authors = stored_work.authors.clone();
    }
    if locked.contains("entry_type") {
        work.entry_override = stored_work
            .entry_override
            .clone()
            .or(Some(stored_work.source_type.clone()));
    }
    if stored_work.citation_key.is_some() {
        work.citation_key = stored_work.citation_key.clone();
    }
    copy_locked(&mut work, &stored_work, &locked);
    if locked.contains("editor") {
        work.editors = stored_work.editors.clone();
    }
    if work.doi.is_empty() {
        work.doi = stored_work.doi;
    }
    if work.year.is_none() {
        work.year = stored_work.year;
    }
    if work.title.is_empty() {
        work.title = stored_work.title;
    }
    work
}

fn copy_locked(work: &mut Work, stored: &Work, locked: &HashSet<String>) {
    let take = |flag: bool, current: &mut Option<String>, replacement: &Option<String>| {
        if flag {
            *current = replacement.clone();
        }
    };
    take(
        locked.contains("journal"),
        &mut work.journal,
        &stored.journal,
    );
    take(
        locked.contains("booktitle"),
        &mut work.booktitle,
        &stored.booktitle,
    );
    take(
        locked.contains("chapter"),
        &mut work.chapter,
        &stored.chapter,
    );
    take(locked.contains("volume"), &mut work.volume, &stored.volume);
    take(locked.contains("issue"), &mut work.issue, &stored.issue);
    take(locked.contains("pages"), &mut work.pages, &stored.pages);
    take(locked.contains("eid"), &mut work.eid, &stored.eid);
    take(
        locked.contains("publisher"),
        &mut work.publisher,
        &stored.publisher,
    );
    take(
        locked.contains("institution"),
        &mut work.institution,
        &stored.institution,
    );
    if locked.contains("doi") {
        work.doi = stored.doi.clone();
    }
    take(locked.contains("url"), &mut work.url, &stored.url);
    take(
        locked.contains("version"),
        &mut work.version,
        &stored.version,
    );
    take(locked.contains("note"), &mut work.note, &stored.note);
    take(
        locked.contains("howpublished"),
        &mut work.howpublished,
        &stored.howpublished,
    );
    work.note_locked = locked.contains("note");
    work.howpublished_locked = locked.contains("howpublished");
    take(
        locked.contains("number"),
        &mut work.report_number,
        &stored.report_number,
    );
    if locked.contains("year") {
        work.year = stored.year;
    }
}

fn authors_cover(stored: &[Person], provider: &[Person]) -> bool {
    let have: HashSet<String> = provider.iter().map(family_key).collect();
    stored
        .iter()
        .all(|person| have.contains(&family_key(person)))
}

fn family_key(person: &Person) -> String {
    fold_ascii(&person.family).to_ascii_lowercase()
}

fn user_fields(meta: &Value) -> HashSet<String> {
    meta.get("user_fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(|field| field.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn work_from_provider_record(record: &Value) -> Option<Work> {
    let mut work = if let Some(message) = record.get("crossref").filter(|v| v.is_object()) {
        let mut parsed = work_from_crossref(message);
        parsed.from_crossref = true;
        parsed
    } else {
        let attrs = record.get("datacite").filter(|value| value.is_object())?;
        work_from_datacite(attrs)
    };
    if let Some(zenodo) = record.get("zenodo").filter(|v| v.is_object()) {
        apply_zenodo_payload(&mut work, zenodo);
    }
    Some(work)
}

fn work_from_crossref(message: &Value) -> Work {
    let mut work = Work::empty();
    work.source_type = text(message, "type").unwrap_or_default();
    work.title = first_string(message.get("title")).unwrap_or_default();
    work.authors = people(message.get("author"));
    work.editors = people(message.get("editor"));
    if work.authors.is_empty() && !work.editors.is_empty() {
        work.authors = work.editors.clone();
    }
    work.year =
        crossref_year(message).or_else(|| year_in_doi(&text(message, "DOI").unwrap_or_default()));
    let containers = string_list(message.get("container-title"));
    let event = message
        .get("event")
        .and_then(|event| text(event, "name").or_else(|| text(event, "acronym")));
    let kind = map_type(&work.source_type, false);
    match kind {
        "inproceedings" | "incollection" => {
            work.booktitle = choose_booktitle(&containers, kind).or(event);
        }
        "article" => {
            work.journal = containers.first().cloned();
        }
        _ => {}
    }
    work.publisher = text(message, "publisher");
    work.volume = text(message, "volume");
    work.issue = text(message, "issue");
    work.pages = text(message, "page");
    work.eid = text(message, "article-number");
    // PeerJ and MethodsX repeat the article number in `page`. Keep it once, as pages.
    if work.pages.is_some() && work.pages == work.eid {
        work.eid = None;
    }
    work.doi = text(message, "DOI")
        .map(|doi| doi.to_ascii_lowercase())
        .unwrap_or_default();
    let landing = message
        .pointer("/resource/primary/URL")
        .and_then(Value::as_str)
        .map(str::to_string);
    let url = text(message, "URL");
    work.url = landing.or(url);
    if kind == "techreport" {
        if let Some(number) = rfc_number(&work.doi) {
            work.report_number = Some(number);
            work.institution = Some("IETF".into());
        } else {
            work.institution = work.publisher.clone();
        }
    }
    if work.source_type == "book-chapter" && work.chapter.is_none() {
        work.chapter = chapter_from_doi(&work.doi);
    }
    if work.source_type == "standard" {
        if let Some(body) = message.get("standards-body") {
            let name = text(body, "name").unwrap_or_default();
            let org = if name.contains("IEEE Computer Society") {
                "IEEE Computer Society".to_string()
            } else {
                name
            };
            if !org.is_empty() {
                work.authors = vec![Person {
                    family: org,
                    given: String::new(),
                    literal: true,
                }];
            }
        }
        if work.year.is_none() {
            work.year = year_in_doi(&work.doi);
        }
    }
    if work.source_type == "preprint" || work.source_type == "posted-content" {
        work.howpublished = arxiv_label(&work.doi);
    }
    soften_people(&mut work.authors);
    soften_people(&mut work.editors);
    work
}

fn work_from_datacite(attrs: &Value) -> Work {
    let mut work = Work::empty();
    let general = attrs
        .pointer("/types/resourceTypeGeneral")
        .and_then(Value::as_str)
        .unwrap_or("");
    work.source_type = match general.to_ascii_lowercase().as_str() {
        "software" => "software".into(),
        "preprint" => "preprint".into(),
        "journalarticle" => "journal-article".into(),
        "conferencepaper" => "proceedings-article".into(),
        "bookchapter" => "book-chapter".into(),
        "report" => "report".into(),
        other => other.to_string(),
    };
    work.title = attrs
        .get("titles")
        .and_then(Value::as_array)
        .and_then(|titles| titles.first())
        .and_then(|title| text(title, "title"))
        .unwrap_or_default();
    work.authors = datacite_people(attrs.get("creators"));
    work.year = attrs
        .get("publicationYear")
        .and_then(Value::as_i64)
        .map(|year| year as i32)
        .or_else(|| {
            attrs
                .get("publicationYear")
                .and_then(Value::as_str)
                .and_then(|year| year.parse().ok())
        });
    work.publisher = text(attrs, "publisher");
    work.doi = text(attrs, "doi")
        .map(|doi| doi.to_ascii_lowercase())
        .unwrap_or_default();
    if work.source_type == "preprint" {
        work.howpublished = arxiv_label(&work.doi);
    }
    label_zenodo_relations(&mut work, attrs);
    work
}

fn label_zenodo_relations(work: &mut Work, attrs: &Value) {
    if !is_zenodo_doi(&work.doi) {
        return;
    }
    let mut version_of = None;
    let mut has_version = false;
    if let Some(rels) = attrs.get("relatedIdentifiers").and_then(Value::as_array) {
        for rel in rels {
            match rel.get("relationType").and_then(Value::as_str) {
                Some("IsVersionOf") => {
                    version_of = text(rel, "relatedIdentifier");
                }
                Some("HasVersion") => has_version = true,
                _ => {}
            }
        }
    }
    let version = text(attrs, "version");
    if let Some(concept) = version_of {
        work.version = version;
        work.zenodo = Some(ZenodoRecord::Version);
        work.note = Some(format!("Zenodo version DOI; concept {concept}"));
    } else if has_version {
        work.version = None;
        work.zenodo = Some(ZenodoRecord::Concept);
        work.note = Some(match version {
            Some(latest) => format!("Zenodo concept DOI; latest version {latest}"),
            None => "Zenodo concept DOI".into(),
        });
    }
}

fn apply_zenodo_payload(work: &mut Work, zenodo: &Value) {
    let concept = text(zenodo, "conceptdoi").unwrap_or_default();
    let version_doi = text(zenodo, "doi").unwrap_or_default();
    let version = text(zenodo, "version").or_else(|| {
        zenodo
            .pointer("/metadata/version")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let requested = work.doi.to_ascii_lowercase();
    if !concept.is_empty()
        && requested == concept.to_ascii_lowercase()
        && !concept.eq_ignore_ascii_case(&version_doi)
    {
        work.doi = concept.to_ascii_lowercase();
        work.version = None;
        work.zenodo = Some(ZenodoRecord::Concept);
        work.note = Some(match &version {
            Some(latest) => format!("Zenodo concept DOI; latest version {latest}"),
            None => "Zenodo concept DOI".into(),
        });
    } else if !version_doi.is_empty() && requested == version_doi.to_ascii_lowercase() {
        work.doi = version_doi.to_ascii_lowercase();
        work.version = version.clone();
        work.zenodo = Some(ZenodoRecord::Version);
        work.note = Some(if concept.is_empty() {
            "Zenodo version DOI".into()
        } else {
            format!(
                "Zenodo version DOI; concept {}",
                concept.to_ascii_lowercase()
            )
        });
    }
}

fn work_from_stored(meta: &Value) -> Work {
    let mut work = Work::empty();
    work.source_type = text(meta, "entry_type").unwrap_or_default();
    if user_fields(meta).contains("entry_type") && !work.source_type.is_empty() {
        work.entry_override = Some(work.source_type.clone());
    }
    work.title = text(meta, "title").unwrap_or_default();
    work.authors = people(meta.get("authors"));
    work.editors = people(meta.get("editor"));
    work.year = meta
        .pointer("/date/year")
        .and_then(Value::as_i64)
        .or_else(|| meta.get("year").and_then(Value::as_i64))
        .map(|year| year as i32);
    work.journal = text(meta, "journal");
    work.booktitle = text(meta, "booktitle");
    work.chapter = text(meta, "chapter");
    work.volume = text(meta, "volume");
    work.issue = text(meta, "issue");
    work.report_number = text(meta, "number");
    work.pages = text(meta, "pages");
    work.eid = text(meta, "eid");
    work.publisher = text(meta, "publisher");
    work.institution = text(meta, "institution");
    work.doi = text(meta, "doi")
        .map(|doi| doi.trim().trim_start_matches("doi:").to_ascii_lowercase())
        .unwrap_or_default();
    work.url = text(meta, "url");
    work.version = text(meta, "version");
    work.note = text(meta, "note");
    work.howpublished = text(meta, "howpublished");
    work.citation_key = text(meta, "citation_key").or_else(|| text(meta, "cite_key"));
    let locked = user_fields(meta);
    work.note_locked = locked.contains("note");
    work.howpublished_locked = locked.contains("howpublished");
    let kind = map_type(&work.source_type, false);
    if work.booktitle.is_none() && matches!(kind, "inproceedings" | "incollection" | "inbook") {
        work.booktitle = work.journal.take();
    }
    work
}

fn choose_booktitle(containers: &[String], kind: &str) -> Option<String> {
    if containers.is_empty() {
        return None;
    }
    if kind == "incollection" {
        let specific: Vec<&String> = containers
            .iter()
            .filter(|title| !is_series(title))
            .collect();
        if specific.len() == 1 {
            return Some(specific[0].clone());
        }
        if !specific.is_empty() {
            return specific.last().cloned().cloned();
        }
    }
    containers.first().cloned()
}

fn is_series(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    lower.contains("series") || lower.contains("lecture notes")
}

fn render_works(works: &[Work], options: &BibOptions) -> String {
    let mut used = HashSet::new();
    let mut out = String::new();
    for work in works {
        let custom = work
            .citation_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string);
        let mut key = custom.clone().unwrap_or_else(|| generated_key(work));
        if key.is_empty() {
            key = "ref".into();
        }
        if used.contains(&key) {
            let base = key.clone();
            let mut suffix = b'a';
            while used.contains(&key) {
                key = format!("{base}{}", suffix as char);
                suffix = suffix.saturating_add(1);
            }
        }
        used.insert(key.clone());
        out.push_str(&render_entry(work, &key, options));
        out.push('\n');
    }
    out
}

fn render_entry(work: &Work, key: &str, options: &BibOptions) -> String {
    let bib_type = if let Some(override_type) = work.entry_override.as_deref() {
        map_type(override_type, options.software_as_misc)
    } else {
        map_type(&work.source_type, options.software_as_misc)
    };
    let software_misc = options.software_as_misc
        && bib_type == "misc"
        && map_type(&work.source_type, false) == "software";
    let mut fields = Vec::new();
    if !work.title.is_empty() {
        push_raw(&mut fields, "title", &display_title(&work.title));
    }
    if let Some(author) = format_people(&work.authors) {
        fields.push(format!("  author = {{{author}}}"));
    }
    if let Some(year) = work.year {
        fields.push(format!("  year = {{{year}}}"));
    }
    match bib_type {
        "inproceedings" | "incollection" | "inbook" => {
            if let Some(booktitle) = &work.booktitle {
                push_raw(&mut fields, "booktitle", &display_title(booktitle));
            }
        }
        _ => {
            if let Some(journal) = &work.journal {
                push_text(&mut fields, "journal", journal);
            }
        }
    }
    if bib_type == "incollection" || bib_type == "inbook" {
        if let Some(chapter) = &work.chapter {
            push_plain(&mut fields, "chapter", chapter);
        }
    }
    if let Some(volume) = &work.volume {
        push_plain(&mut fields, "volume", volume);
    }
    let number = if bib_type == "techreport" {
        work.report_number.clone().or_else(|| work.issue.clone())
    } else {
        work.issue.clone().or_else(|| work.report_number.clone())
    };
    if let Some(number) = number {
        push_plain(&mut fields, "number", &number);
    }
    let mut pages = work.pages.clone();
    let mut eid = work.eid.clone();
    let mut eid_note = None;
    if pages.is_none() && eid.is_some() {
        match options.eid_as {
            EidPlacement::Eid => {}
            EidPlacement::Pages => pages = eid.take(),
            EidPlacement::Note => eid_note = eid.take().map(|number| format!("Article {number}")),
        }
    } else if options.eid_as != EidPlacement::Eid {
        // The page range already locates the article; a style that ignores
        // `eid` gains nothing from a second locator.
        eid = None;
    }
    if let Some(pages) = &pages {
        push_plain(&mut fields, "pages", &normalize_pages(pages));
    }
    if let Some(eid) = &eid {
        push_plain(&mut fields, "eid", eid);
    }
    let mut howpublished = work.howpublished.clone();
    let mut note = work.note.clone();
    let mut publisher = work.publisher.clone();
    if software_misc {
        if !work.howpublished_locked {
            howpublished = Some(publisher.take().unwrap_or_else(|| "Software".into()));
        }
        if !work.note_locked {
            note = software_note(work, howpublished.as_deref());
        }
    }
    if let Some(publisher) = &publisher {
        push_text(&mut fields, "publisher", publisher);
    }
    if bib_type == "techreport" {
        if let Some(institution) = &work.institution {
            push_text(&mut fields, "institution", institution);
        }
    }
    if let Some(version) = &work.version {
        push_plain(&mut fields, "version", version);
    }
    if bib_type == "misc" {
        if let Some(howpublished) = howpublished {
            push_text(&mut fields, "howpublished", &howpublished);
        }
    }
    let note = match (note, eid_note) {
        (Some(note), Some(extra)) => Some(format!("{note}, {extra}")),
        (note, extra) => note.or(extra),
    };
    if let Some(note) = &note {
        push_text(&mut fields, "note", note);
    }
    if !work.editors.is_empty() && (work.editors != work.authors || bib_type == "techreport") {
        if let Some(editor) = format_people(&work.editors) {
            fields.push(format!("  editor = {{{editor}}}"));
        }
    }
    if !work.doi.is_empty() {
        push_plain(&mut fields, "doi", &work.doi);
    }
    if let Some(url) = work
        .url
        .as_deref()
        .filter(|url| !url_repeats_doi(url, &work.doi))
    {
        push_plain(&mut fields, "url", url);
    }
    format!("@{bib_type}{{{key},\n{}\n}}\n", fields.join(",\n"))
}

/// `note` of a software record written as `@misc`: what it is and which
/// version, without the archive name already given in `howpublished`.
fn software_note(work: &Work, howpublished: Option<&str>) -> Option<String> {
    let label = if howpublished.is_some_and(|text| text.eq_ignore_ascii_case("software")) {
        None
    } else {
        Some("Software")
    };
    let detail = match (&work.zenodo, &work.version) {
        (Some(ZenodoRecord::Concept), _) => Some("all versions".to_string()),
        (_, Some(version)) => Some(format!("version {}", version.trim_start_matches('v'))),
        _ => None,
    };
    match (label, detail) {
        (Some(label), Some(detail)) => Some(format!("{label}, {detail}")),
        (Some(label), None) => Some(label.to_string()),
        (None, Some(detail)) => Some(detail),
        (None, None) => None,
    }
}

/// A field whose value is already BibTeX (a protected title).
fn push_raw(fields: &mut Vec<String>, name: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    fields.push(format!("  {name} = {{{value}}}"));
}

/// A free-text field: entities decoded, markup and stray spacing removed,
/// dashes normalised, and BibTeX specials escaped.
fn push_text(fields: &mut Vec<String>, name: &str, value: &str) {
    let value = escape_bib_text(&clean_text(value));
    push_raw(fields, name, &value);
}

fn push_plain(fields: &mut Vec<String>, name: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    fields.push(format!("  {name} = {{{value}}}"));
}

fn format_people(people: &[Person]) -> Option<String> {
    let names: Vec<String> = people
        .iter()
        .filter_map(|person| {
            if person.family.trim().is_empty() {
                return None;
            }
            let family = clean_name(&person.family);
            let given = clean_name(&person.given);
            if person.literal {
                Some(format!("{{{family}}}"))
            } else if given.is_empty() {
                Some(family)
            } else {
                Some(format!(
                    "{family}, {}",
                    tex_initial_letters(&format_initials(&given))
                ))
            }
        })
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(" and "))
    }
}

/// BibTeX abbreviates a given name by its first byte, which splits a UTF-8
/// letter ("Ádám" becomes an invalid byte and a dot). An accented capital
/// that starts a given-name part is written as a braced TeX accent instead.
fn tex_initial_letters(given: &str) -> String {
    let mut out = String::new();
    let mut at_start = true;
    for ch in given.chars() {
        if at_start {
            if let Some(tex) = tex_accent(ch) {
                out.push_str(tex);
                at_start = false;
                continue;
            }
        }
        out.push(ch);
        at_start = ch.is_whitespace() || ch == '-' || ch == '.';
    }
    out
}

fn tex_accent(ch: char) -> Option<&'static str> {
    Some(match ch {
        'Á' => "{\\'A}",
        'À' => "{\\`A}",
        'Â' => "{\\^A}",
        'Ä' => "{\\\"A}",
        'Å' => "{\\AA}",
        'Æ' => "{\\AE}",
        'Ç' => "{\\c{C}}",
        'Č' => "{\\v{C}}",
        'É' => "{\\'E}",
        'È' => "{\\`E}",
        'Ê' => "{\\^E}",
        'Ë' => "{\\\"E}",
        'Í' => "{\\'I}",
        'Î' => "{\\^I}",
        'Ł' => "{\\L}",
        'Ñ' => "{\\~N}",
        'Ó' => "{\\'O}",
        'Ô' => "{\\^O}",
        'Ö' => "{\\\"O}",
        'Ø' => "{\\O}",
        'Ř' => "{\\v{R}}",
        'Š' => "{\\v{S}}",
        'Ś' => "{\\'S}",
        'Ú' => "{\\'U}",
        'Ü' => "{\\\"U}",
        'Ý' => "{\\'Y}",
        'Ž' => "{\\v{Z}}",
        _ => return None,
    })
}

/// A name with entities decoded, Unicode hyphens as `-`, and BibTeX specials escaped.
fn clean_name(value: &str) -> String {
    escape_bib_text(&normalize_spacing(&normalize_dashes(&decode_entities(
        value.trim(),
    ))))
}

fn generated_key(work: &Work) -> String {
    let family = work
        .authors
        .first()
        .map(|person| fold_ascii(&person.family).to_ascii_lowercase())
        .filter(|family| !family.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let words = title_key_words(&work.title);
    let mut key = family;
    key.push_str(&words);
    if let Some(year) = work.year {
        key.push_str(&year.to_string());
    }
    if key.is_empty() {
        "ref".into()
    } else {
        key
    }
}

fn title_key_words(title: &str) -> String {
    const STOP: &[&str] = &[
        "a", "an", "the", "of", "and", "for", "in", "on", "to", "with", "via", "from", "by", "or",
        "at", "as", "its",
    ];
    let mut out = String::new();
    let mut count = 0;
    for raw in title.split_whitespace() {
        if count == 3 {
            break;
        }
        let folded = fold_ascii(raw).to_ascii_lowercase();
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

fn map_type(raw: &str, software_as_misc: bool) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "journal-article" | "article" | "article-journal" => "article",
        "proceedings-article"
        | "inproceedings"
        | "conference"
        | "paper-conference"
        | "proceedings" => "inproceedings",
        "book-chapter" | "incollection" | "chapter" => "incollection",
        "inbook" => "inbook",
        "book" | "monograph" | "book-monograph" => "book",
        "report" | "report-technical" | "techreport" => "techreport",
        "preprint" | "posted-content" => "misc",
        "software" | "softwaresourcecode" => {
            if software_as_misc {
                "misc"
            } else {
                "software"
            }
        }
        "phdthesis" | "thesis" => "phdthesis",
        "patent" => "patent",
        "" => "article",
        "standard" | "misc" | "online" | "webpage" | "dataset" | "text" => "misc",
        _ => "misc",
    }
}

fn display_title(raw: &str) -> String {
    let cleaned = clean_text(raw);
    let cased = if is_shouting(&cleaned) {
        title_case(&cleaned)
    } else {
        cleaned
    };
    protect_case(&escape_bib_text(&cased))
}

fn is_shouting(value: &str) -> bool {
    let mut upper = false;
    for ch in value.chars() {
        if ch.is_lowercase() {
            return false;
        }
        if ch.is_uppercase() {
            upper = true;
        }
    }
    upper
}

fn title_case(title: &str) -> String {
    const SMALL: &[&str] = &[
        "a", "an", "the", "of", "and", "for", "in", "on", "to", "with", "via", "from", "by", "or",
        "at", "as",
    ];
    let words: Vec<&str> = title.split_whitespace().collect();
    let last = words.len().saturating_sub(1);
    words
        .iter()
        .enumerate()
        .map(|(index, word)| {
            let letters: String = word
                .chars()
                .filter(|ch| ch.is_alphabetic())
                .flat_map(|ch| ch.to_lowercase())
                .collect();
            let small = index != 0 && index != last && SMALL.contains(&letters.as_str());
            recase_word(word, !small)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn recase_word(word: &str, capitalize: bool) -> String {
    let mut out = String::new();
    let mut started = false;
    for ch in word.chars() {
        if !started && ch.is_alphabetic() {
            if capitalize {
                out.extend(ch.to_uppercase());
            } else {
                out.extend(ch.to_lowercase());
            }
            started = true;
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// Brace what a sentence-casing style must not lowercase.
///
/// Acronyms, internal capitals, and names stay braced. In a sentence-case
/// title any other capitalised word after the first is a name, so it is
/// braced too. In a title-case title those capitals are only title casing,
/// so only known names are braced and the style can lowercase the rest; that
/// keeps every reference in one case. Brace groups already in the title are
/// copied unchanged.
fn protect_case(title: &str) -> String {
    let title_cased = is_title_cased(title);
    let mut out = String::new();
    let mut word = String::new();
    let mut first_word = true;
    let chars: Vec<char> = title.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '{' {
            flush_word(&mut word, &mut out, &mut first_word, title_cased);
            let mut depth = 0;
            while index < chars.len() {
                let inner = chars[index];
                out.push(inner);
                index += 1;
                if inner == '{' {
                    depth += 1;
                } else if inner == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            first_word = false;
            continue;
        }
        if ch.is_alphanumeric() || ch == '\'' || ch == '-' {
            word.push(ch);
        } else {
            flush_word(&mut word, &mut out, &mut first_word, title_cased);
            out.push(ch);
            // A sentence-casing style keeps the capital after a colon.
            if ch == ':' {
                first_word = true;
            }
        }
        index += 1;
    }
    flush_word(&mut word, &mut out, &mut first_word, title_cased);
    out
}

fn flush_word(word: &mut String, out: &mut String, first_word: &mut bool, title_cased: bool) {
    if word.is_empty() {
        return;
    }
    let lower = word.to_lowercase();
    if let Some(proper) = LOWERCASE_NAMES
        .iter()
        .find(|(plain, _)| *plain == lower && word.as_str() == lower)
        .map(|(_, proper)| *proper)
    {
        out.push('{');
        out.push_str(proper);
        out.push('}');
    } else if needs_brace(word, *first_word, title_cased) {
        out.push('{');
        out.push_str(word);
        out.push('}');
    } else {
        out.push_str(word);
    }
    *first_word = false;
    word.clear();
}

/// Names a publisher sometimes prints in lowercase inside a title.
const LOWERCASE_NAMES: &[(&str, &str)] = &[
    ("python", "Python"),
    ("gaussian", "Gaussian"),
    ("bayesian", "Bayesian"),
    ("hessian", "Hessian"),
    ("newtonian", "Newtonian"),
];

/// Capitalised words that stay capitalised in a title-case title.
const NAMES: &[&str] = &[
    "Arrhenius",
    "Bayesian",
    "Boltzmann",
    "Born",
    "Brownian",
    "Carlo",
    "Cartesian",
    "Cholesky",
    "Dirac",
    "Euler",
    "Ewald",
    "Eyring",
    "Fock",
    "Fortran",
    "Fourier",
    "Gaussian",
    "Hamiltonian",
    "Hartree",
    "Hessian",
    "Hubbard",
    "Ising",
    "Jones",
    "Julia",
    "Kohn",
    "Kramers",
    "Krylov",
    "Lagrangian",
    "Lanczos",
    "Langevin",
    "Laplace",
    "Lennard",
    "Markov",
    "Monte",
    "Newton",
    "Oppenheimer",
    "Poisson",
    "Python",
    "Raman",
    "Rust",
    "Schrödinger",
    "Sham",
    "Verlet",
    "Wannier",
];

/// Most longer words after the first start with a capital.
fn is_title_cased(title: &str) -> bool {
    let mut capital = 0;
    let mut total = 0;
    let mut depth = 0i32;
    let mut word = String::new();
    let mut words = Vec::new();
    for ch in title.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        if ch.is_alphabetic() || ch == '-' || ch == '\'' {
            word.push(ch);
        } else {
            if !word.is_empty() {
                words.push((std::mem::take(&mut word), depth > 0 || ch == '}'));
            }
        }
    }
    if !word.is_empty() {
        words.push((word, false));
    }
    for (word, _) in words.iter().skip(1) {
        let letters = word.chars().filter(|ch| ch.is_alphabetic()).count();
        if letters < 4 {
            continue;
        }
        total += 1;
        if word.chars().next().is_some_and(char::is_uppercase) {
            capital += 1;
        }
    }
    total >= 2 && capital * 10 >= total * 6
}

fn needs_brace(word: &str, first: bool, title_cased: bool) -> bool {
    let letters: Vec<char> = word.chars().filter(|ch| ch.is_alphabetic()).collect();
    if letters.is_empty() {
        return false;
    }
    let uppers = letters.iter().filter(|ch| ch.is_uppercase()).count();
    let has_digit = word.chars().any(|ch| ch.is_ascii_digit());
    // In a title-case title "Energy-Weighted" is two capitalised words, not a name.
    let parts: Vec<&str> = if title_cased {
        word.split('-').collect()
    } else {
        vec![word]
    };
    let internal_upper = parts.iter().any(|part| {
        part.chars()
            .skip_while(|ch| !ch.is_alphabetic())
            .skip(1)
            .any(|ch| ch.is_uppercase())
    });
    let part_uppers = parts
        .iter()
        .any(|part| part.chars().filter(|ch| ch.is_uppercase()).count() >= 2);
    if part_uppers || internal_upper || (has_digit && uppers >= 1) {
        return true;
    }
    if first || !letters.first().is_some_and(|ch| ch.is_uppercase()) {
        return false;
    }
    if !title_cased {
        return true;
    }
    parts.iter().any(|part| NAMES.contains(part))
}

fn escape_bib_text(value: &str) -> String {
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

/// Plain text of a provider string: HTML entities decoded, JATS, HTML and
/// MathML markup removed, whitespace collapsed, and dashes in TeX form.
///
/// Software names set in `<tt>`, `<monospace>`, `<code>`, or `<sc>`/`<scp>`
/// are braced so their case survives. A small-caps run that continues a
/// capital (`P<scp>ACKMOL</scp>`) is joined to it.
fn clean_text(value: &str) -> String {
    let decoded = decode_entities(value);
    let without_tags = strip_tags(&decoded);
    normalize_spacing(&normalize_dashes(&without_tags))
}

fn strip_tags(value: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = value.chars().collect();
    let mut index = 0;
    let mut name_tags: Vec<bool> = Vec::new();
    let mut run = String::new();
    while index < chars.len() {
        let ch = chars[index];
        if ch != '<' {
            if name_tags.last().copied().unwrap_or(false) {
                run.push(ch);
            } else {
                out.push(ch);
            }
            index += 1;
            continue;
        }
        let Some(close) = chars[index..].iter().position(|ch| *ch == '>') else {
            out.push(ch);
            index += 1;
            continue;
        };
        let tag: String = chars[index + 1..index + close].iter().collect();
        index += close + 1;
        let tag = tag.trim();
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|ch: char| ch.is_whitespace() || ch == '/')
            .next()
            .unwrap_or("")
            .rsplit(':')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_name = matches!(name.as_str(), "tt" | "monospace" | "code" | "sc" | "scp");
        if tag.ends_with('/') {
            continue;
        }
        if !closing {
            name_tags.push(is_name);
            continue;
        }
        let was_name = name_tags.pop().unwrap_or(false);
        if !was_name || name_tags.last().copied().unwrap_or(false) {
            continue;
        }
        let text = std::mem::take(&mut run).trim().to_string();
        if text.is_empty() {
            continue;
        }
        let trimmed_len = out.trim_end().len();
        let before: Vec<char> = out[..trimmed_len].chars().collect();
        let joins_capital = name.starts_with("sc")
            && before.last().is_some_and(|ch| ch.is_uppercase())
            && (before.len() == 1 || !before[before.len() - 2].is_alphanumeric());
        if joins_capital {
            let capital = before[before.len() - 1];
            out.truncate(trimmed_len - capital.len_utf8());
            out.push('{');
            out.push(capital);
            out.push_str(&text.to_uppercase());
            out.push('}');
        } else {
            out.push('{');
            out.push_str(&text);
            out.push('}');
        }
    }
    out.push_str(&run);
    out
}

fn decode_entities(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail[1..]
            .find(|ch: char| ch == ';' || ch.is_whitespace() || ch == '&')
            .map(|offset| offset + 1);
        let decoded = match end {
            Some(end) if tail[end..].starts_with(';') => {
                entity(&tail[1..end]).map(|ch| (ch, end + 1))
            }
            _ => None,
        };
        match decoded {
            Some((ch, used)) => {
                out.push_str(&ch);
                rest = &tail[used..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<String> {
    let named = match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "ndash" => "–",
        "mdash" => "—",
        "rsquo" => "’",
        "lsquo" => "‘",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_string());
    }
    let code = name.strip_prefix('#')?;
    let number = if let Some(hex) = code.strip_prefix('x').or_else(|| code.strip_prefix('X')) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        code.parse().ok()?
    };
    char::from_u32(number).map(String::from)
}

/// TeX dashes: an em dash is `---`, an en dash or a spaced hyphen between
/// words is `--`, and Unicode hyphens are `-`.
fn normalize_dashes(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '—' | '―' => out.push_str("---"),
            '–' => out.push_str("--"),
            '‐' | '‑' | '‒' => out.push('-'),
            _ => out.push(ch),
        }
    }
    out.replace(" - ", " -- ")
}

/// One space between words, none before closing punctuation, none after an opening bracket.
fn normalize_spacing(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for ch in collapsed.chars() {
        if matches!(ch, ':' | ';' | ',' | '.' | ')' | '?' | '!') && out.ends_with(' ') {
            out.pop();
        }
        if ch == ' ' && (out.ends_with('(') || out.ends_with('[')) {
            continue;
        }
        out.push(ch);
    }
    out
}

fn normalize_pages(pages: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = pages.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '-' {
            while index < chars.len() && chars[index] == '-' {
                index += 1;
            }
            out.push_str("--");
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

fn url_repeats_doi(url: &str, doi: &str) -> bool {
    if doi.is_empty() {
        return false;
    }
    let url = url.trim().trim_end_matches('/').to_ascii_lowercase();
    let doi = doi.trim().trim_start_matches("doi:").to_ascii_lowercase();
    let markers = [
        format!("doi.org/{doi}"),
        format!("doi.org/doi/{doi}"),
        format!("dx.doi.org/{doi}"),
        format!("zenodo.org/doi/{doi}"),
    ];
    markers.iter().any(|marker| url.contains(marker.as_str()))
}

fn people(value: Option<&Value>) -> Vec<Person> {
    let Some(list) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter().filter_map(person_from_json).collect()
}

fn person_from_json(value: &Value) -> Option<Person> {
    if value.get("literal").and_then(Value::as_bool) == Some(true) {
        let name = text(value, "family")
            .or_else(|| text(value, "name"))
            .unwrap_or_default();
        let name = unwrap_braces(&name).unwrap_or(name);
        if name.is_empty() {
            return None;
        }
        return Some(Person {
            family: name,
            given: String::new(),
            literal: true,
        });
    }
    if let Some(family) = text(value, "family") {
        if let Some(inner) = unwrap_braces(&family) {
            return Some(Person {
                family: inner,
                given: String::new(),
                literal: true,
            });
        }
        return Some(Person {
            family,
            given: text(value, "given").unwrap_or_default(),
            literal: false,
        });
    }
    // Crossref writes an organisation author as a bare `name`.
    if let Some(name) = text(value, "name") {
        let name = unwrap_braces(&name).unwrap_or(name);
        return Some(Person {
            family: name,
            given: String::new(),
            literal: true,
        });
    }
    None
}

fn datacite_people(value: Option<&Value>) -> Vec<Person> {
    let Some(list) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|creator| {
            let name_type = text(creator, "nameType").unwrap_or_default();
            if name_type.eq_ignore_ascii_case("organizational") {
                let name = text(creator, "name").or_else(|| text(creator, "familyName"))?;
                return Some(Person {
                    family: name,
                    given: String::new(),
                    literal: true,
                });
            }
            let given = text(creator, "givenName").unwrap_or_default();
            let family = text(creator, "familyName").unwrap_or_default();
            let display = text(creator, "name").unwrap_or_default();
            if given.is_empty() && (family.is_empty() || family == display) && !display.is_empty() {
                if looks_like_login(&display) {
                    return None;
                }
                return person_from_display(&display);
            }
            if looks_like_login(&family) || looks_like_login(&display) {
                return None;
            }
            if family.is_empty() {
                return person_from_display(&display);
            }
            Some(Person {
                family,
                given,
                literal: false,
            })
        })
        .collect()
}

fn person_from_display(name: &str) -> Option<Person> {
    let name = name.trim();
    if name.is_empty() || looks_like_login(name) {
        return None;
    }
    if let Some(inner) = unwrap_braces(name) {
        return Some(Person {
            family: inner,
            given: String::new(),
            literal: true,
        });
    }
    if let Some((family, given)) = name.split_once(',') {
        let family = family.trim();
        let given = given.trim();
        if family.is_empty() {
            return None;
        }
        return Some(Person {
            family: family.to_string(),
            given: given.to_string(),
            literal: false,
        });
    }
    if let Some((given, family)) = name.rsplit_once(char::is_whitespace) {
        return Some(Person {
            family: family.trim().to_string(),
            given: given.trim().to_string(),
            literal: false,
        });
    }
    Some(Person {
        family: name.to_string(),
        given: String::new(),
        literal: false,
    })
}

fn looks_like_login(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || name.contains(' ') || name.contains(',') {
        return false;
    }
    if name.chars().any(|ch| ch.is_ascii_digit()) || name.contains('-') || name.contains('_') {
        return true;
    }
    name.chars().all(|ch| !ch.is_uppercase()) && name.chars().any(|ch| ch.is_lowercase())
}

fn soften_people(people: &mut [Person]) {
    for person in people.iter_mut() {
        if person.literal {
            continue;
        }
        person.family = soften_shouting(&person.family);
        person.given = soften_shouting(&person.given);
    }
}

fn soften_shouting(value: &str) -> String {
    let letters: Vec<char> = value.chars().filter(|ch| ch.is_alphabetic()).collect();
    if letters.len() < 2 || letters.iter().any(|ch| ch.is_lowercase()) || is_initials(value) {
        return value.to_string();
    }
    recase_word(value, true)
}

/// "S.R.", "S. R.", "K.w." and "J" are initials only.
fn is_initials(value: &str) -> bool {
    let runs: Vec<&str> = value
        .split(|ch: char| ch == '.' || ch.is_whitespace() || ch == '-')
        .filter(|run| !run.is_empty())
        .collect();
    !runs.is_empty() && runs.iter().all(|run| run.chars().count() == 1)
}

/// Initials as "S. R.": upper case, each followed by a full stop and a space.
fn format_initials(value: &str) -> String {
    if !is_initials(value) || !value.contains('.') {
        return value.to_string();
    }
    let mut out = Vec::new();
    let mut hyphen_next = false;
    let mut current = String::new();
    for ch in value.chars() {
        if ch.is_alphabetic() {
            if hyphen_next {
                current.push('-');
                hyphen_next = false;
            } else if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.extend(ch.to_uppercase());
            current.push('.');
        } else if ch == '-' {
            hyphen_next = true;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out.join(" ")
}

fn unwrap_braces(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches(',').trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.len() > 2 {
        Some(trimmed[1..trimmed.len() - 1].trim().to_string())
    } else {
        None
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn first_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| {
            value
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn crossref_year(message: &Value) -> Option<i32> {
    for key in ["issued", "published-print", "published-online", "published"] {
        if let Some(year) = message.get(key).and_then(year_from_date_parts) {
            return Some(year);
        }
    }
    None
}

fn year_from_date_parts(value: &Value) -> Option<i32> {
    let year = value
        .get("date-parts")
        .and_then(Value::as_array)?
        .first()?
        .as_array()?
        .first()?;
    let year = year.as_i64()?;
    if (1000..3000).contains(&year) {
        Some(year as i32)
    } else {
        None
    }
}

fn year_in_doi(doi: &str) -> Option<i32> {
    let chars: Vec<char> = doi.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_ascii_digit() {
            let start = index;
            while index < chars.len() && chars[index].is_ascii_digit() {
                index += 1;
            }
            if index - start == 4 {
                let token: String = chars[start..index].iter().collect();
                if let Ok(year) = token.parse::<i32>() {
                    if (1900..2100).contains(&year) {
                        return Some(year);
                    }
                }
            }
        } else {
            index += 1;
        }
    }
    None
}

fn rfc_number(doi: &str) -> Option<String> {
    let lower = doi.to_ascii_lowercase();
    let rest = lower.split("rfc").nth(1)?;
    let digits: String = rest
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

fn chapter_from_doi(doi: &str) -> Option<String> {
    let (_, tail) = doi.rsplit_once('_')?;
    if !tail.is_empty() && tail.chars().all(|ch| ch.is_ascii_digit()) {
        Some(tail.trim_start_matches('0').to_string())
            .filter(|chapter| !chapter.is_empty())
            .or(Some("0".into()))
    } else {
        None
    }
}

fn arxiv_label(doi: &str) -> Option<String> {
    let lower = doi.to_ascii_lowercase();
    let id = lower
        .split("arxiv.")
        .nth(1)
        .or_else(|| lower.split("arxiv/").nth(1))?;
    let id = id.trim_matches(|ch: char| ch == '/' || ch.is_whitespace());
    if id.is_empty() {
        None
    } else {
        Some(format!("arXiv:{id}"))
    }
}

fn is_zenodo_doi(doi: &str) -> bool {
    doi.to_ascii_lowercase().starts_with("10.5281/zenodo.")
}

fn fold_ascii(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => out.push('a'),
            'À' | 'Á' | 'Â' | 'Ä' | 'Ã' | 'Å' | 'Ā' => out.push('A'),
            'ç' | 'ć' | 'č' => out.push('c'),
            'Ç' | 'Ć' | 'Č' => out.push('C'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ě' => out.push('e'),
            'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ě' => out.push('E'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' => out.push('i'),
            'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' => out.push('I'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'Ñ' | 'Ń' | 'Ň' => out.push('N'),
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' => out.push('o'),
            'Ò' | 'Ó' | 'Ô' | 'Ö' | 'Õ' | 'Ø' | 'Ō' => out.push('O'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => out.push('u'),
            'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ū' | 'Ů' => out.push('U'),
            'ý' | 'ÿ' => out.push('y'),
            'Ý' => out.push('Y'),
            'ł' => out.push('l'),
            'Ł' => out.push('L'),
            'š' | 'ś' => out.push('s'),
            'Š' | 'Ś' => out.push('S'),
            'ž' | 'ź' | 'ż' => out.push('z'),
            'Ž' | 'Ź' | 'Ż' => out.push('Z'),
            'ř' => out.push('r'),
            'Ř' => out.push('R'),
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'Æ' => out.push_str("AE"),
            'œ' => out.push_str("oe"),
            'ð' | 'þ' => out.push('d'),
            other if other.is_ascii_alphanumeric() => out.push(other),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn bib_of(provider: Value) -> String {
        let stored = json!({"doi": provider.pointer("/crossref/DOI").and_then(|v| v.as_str()).or_else(|| provider.pointer("/datacite/doi").and_then(|v| v.as_str())).unwrap_or("")});
        let work = assemble(&stored, Some(&provider));
        render_works(&[work], &BibOptions::default())
    }

    #[test]
    fn stored_inbook_moves_its_container_into_booktitle() {
        let bib = render_stored_entries(&[json!({
            "entry_type": "inbook",
            "title": "Introduction to the kinetic Monte Carlo method",
            "authors": [{"family": "Voter", "given": "Arthur F."}],
            "year": 2007,
            "journal": "Radiation Effects in Solids",
            "pages": "1-23",
            "publisher": "Springer Netherlands",
            "citation_key": "voterIntroductionKineticMonte2007"
        })]);
        assert!(
            bib.contains("@inbook{voterIntroductionKineticMonte2007,"),
            "{bib}"
        );
        assert!(
            bib.contains("booktitle = {Radiation Effects in Solids}"),
            "{bib}"
        );
        assert!(!bib.contains("\n  journal = "), "{bib}");
        assert!(bib.contains("pages = {1--23}"), "{bib}");
    }

    #[test]
    fn proceedings_use_container_title_as_booktitle() {
        let bib = bib_of(json!({
            "crossref": {
                "type": "proceedings-article",
                "title": ["How to read floating point numbers accurately"],
                "container-title": ["Proceedings of the ACM SIGPLAN 1990 conference on Programming language design and implementation  - PLDI '90"],
                "event": {"name": "the ACM SIGPLAN 1990 conference", "acronym": "PLDI '90"},
                "publisher": "ACM Press",
                "page": "92-101",
                "DOI": "10.1145/93542.93557",
                "URL": "https://doi.org/10.1145/93542.93557",
                "author": [{"given": "William D.", "family": "Clinger"}],
                "issued": {"date-parts": [[1990]]}
            }
        }));
        assert!(bib.starts_with("@inproceedings{"), "{bib}");
        assert!(bib.contains("booktitle = {"), "{bib}");
        assert!(bib.contains("PLDI"), "{bib}");
        assert!(!bib.contains("\n  journal = "), "{bib}");
        assert!(bib.contains("publisher = {ACM Press}"), "{bib}");
        assert!(!bib.contains("url = {https://doi.org/"), "{bib}");
        assert!(bib.contains("clingerHowReadFloating1990"), "{bib}");
    }

    #[test]
    fn event_name_fills_booktitle_when_container_title_is_absent() {
        let bib = bib_of(json!({
            "crossref": {
                "type": "proceedings-article",
                "title": ["A talk"],
                "event": {"name": "Python in Science Conference"},
                "publisher": "SciPy",
                "DOI": "10.1/scipy",
                "author": [{"family": "Beckstein", "given": "Oliver"}],
                "issued": {"date-parts": [[2021]]}
            }
        }));
        assert!(
            bib.contains("booktitle = {{Python} in {Science} {Conference}}")
                || bib.contains("Python"),
            "{bib}"
        );
        assert!(bib.contains("booktitle = "), "{bib}");
        assert!(!bib.contains("booktitle = {SciPy}"), "{bib}");
    }

    #[test]
    fn journal_article_is_not_a_techreport_and_keeps_the_issue() {
        let bib = bib_of(json!({
            "crossref": {
                "type": "journal-article",
                "title": ["A climbing image nudged elastic band method for finding saddle points and minimum energy paths"],
                "container-title": ["The Journal of Chemical Physics"],
                "publisher": "AIP Publishing",
                "volume": "113",
                "issue": "22",
                "page": "9901-9904",
                "DOI": "10.1063/1.1329672",
                "URL": "https://doi.org/10.1063/1.1329672",
                "author": [
                    {"given": "Graeme", "family": "Henkelman"},
                    {"given": "Blas P.", "family": "Uberuaga"},
                    {"given": "Hannes", "family": "Jónsson"}
                ],
                "issued": {"date-parts": [[2000]]}
            }
        }));
        assert!(bib.starts_with("@article{"), "{bib}");
        assert!(bib.contains("number = {22}"), "{bib}");
        assert!(bib.contains("pages = {9901--9904}"), "{bib}");
        assert!(bib.contains("Jónsson") || bib.contains("Jonsson"), "{bib}");
    }

    #[test]
    fn frontiers_article_takes_every_crossref_author_and_the_article_number() {
        let stored = json!({
            "doi": "10.3389/fchem.2026.1807063",
            "entry_type": "techreport",
            "title": "Enhanced climbing image nudged elastic band method with Hessian eigenmode alignment",
            "authors": [{"family": "Goswami", "given": "Rohit"}],
            "journal": "Frontiers in Chemistry",
            "pages": "1807063"
        });
        let provider = json!({
            "crossref": {
                "type": "journal-article",
                "title": ["Enhanced climbing image nudged elastic band method with Hessian eigenmode alignment"],
                "container-title": ["Frontiers in Chemistry"],
                "volume": "14",
                "article-number": "1807063",
                "DOI": "10.3389/fchem.2026.1807063",
                "URL": "https://doi.org/10.3389/fchem.2026.1807063",
                "author": [
                    {"given": "Rohit", "family": "Goswami"},
                    {"given": "Miha", "family": "Gunde"},
                    {"given": "Hannes", "family": "Jónsson"}
                ],
                "issued": {"date-parts": [[2026]]},
                "resource": {"primary": {"URL": "https://www.frontiersin.org/articles/10.3389/fchem.2026.1807063/full"}}
            }
        });
        let bib = render_works(
            &[assemble(&stored, Some(&provider))],
            &BibOptions::default(),
        );
        assert!(bib.starts_with("@article{"), "{bib}");
        assert!(bib.contains("Gunde"), "{bib}");
        assert!(bib.contains("Jónsson") || bib.contains("Jonsson"), "{bib}");
        assert!(bib.contains("eid = {1807063}"), "{bib}");
        assert!(!bib.contains("1807063--1807063"), "{bib}");
        assert!(!bib.contains("number = {1807063}"), "{bib}");
        assert!(bib.contains("{Hessian}"), "{bib}");
        assert!(bib.contains("url = {https://www.frontiersin.org/"), "{bib}");
        assert!(!bib.contains("url = {https://doi.org/"), "{bib}");
        assert!(bib.contains("goswamiEnhancedClimbingImage2026"), "{bib}");
    }

    #[test]
    fn article_number_is_eid_and_issue_is_number() {
        let bib = bib_of(json!({
            "crossref": {
                "type": "journal-article",
                "title": ["Optimization methods for finding minimum energy paths"],
                "container-title": ["The Journal of Chemical Physics"],
                "volume": "128",
                "issue": "13",
                "article-number": "134106",
                "DOI": "10.1063/1.2841941",
                "URL": "https://doi.org/10.1063/1.2841941",
                "author": [{"given": "Daniel", "family": "Sheppard"}],
                "issued": {"date-parts": [[2008]]}
            }
        }));
        assert!(bib.contains("number = {13}"), "{bib}");
        assert!(bib.contains("eid = {134106}"), "{bib}");
        assert!(!bib.contains("134106--134106"), "{bib}");
        assert!(!bib.contains("pages = {134106}"), "{bib}");
    }

    #[test]
    fn book_chapter_is_an_incollection_with_chapter_and_pages() {
        let bib = bib_of(json!({
            "crossref": {
                "type": "book-chapter",
                "title": ["INTRODUCTION TO THE KINETIC MONTE CARLO METHOD"],
                "container-title": ["NATO Science Series", "Radiation Effects in Solids"],
                "publisher": "Springer Netherlands",
                "page": "1-23",
                "DOI": "10.1007/978-1-4020-5295-8_1",
                "URL": "https://doi.org/10.1007/978-1-4020-5295-8_1",
                "author": [{"given": "Arthur F.", "family": "Voter"}]
            }
        }));
        assert!(bib.starts_with("@incollection{"), "{bib}");
        assert!(
            bib.contains("booktitle = {Radiation Effects in Solids}"),
            "{bib}"
        );
        assert!(!bib.contains("NATO Science Series"), "{bib}");
        assert!(bib.contains("chapter = {1}"), "{bib}");
        assert!(bib.contains("pages = {1--23}"), "{bib}");
        // Title case is left to the style; only the names stay braced.
        assert!(bib.contains("Kinetic {Monte} {Carlo} Method"), "{bib}");
    }

    #[test]
    fn rfc_report_has_number_and_ietf_institution() {
        let bib = bib_of(json!({
            "crossref": {
                "type": "report",
                "title": ["The JavaScript Object Notation (JSON) Data Interchange Format"],
                "publisher": "RFC Editor",
                "DOI": "10.17487/rfc8259",
                "URL": "https://doi.org/10.17487/rfc8259",
                "editor": [{"given": "T.", "family": "Bray"}],
                "issued": {"date-parts": [[2017]]},
                "resource": {"primary": {"URL": "https://www.rfc-editor.org/info/rfc8259"}}
            }
        }));
        assert!(bib.starts_with("@techreport{"), "{bib}");
        assert!(bib.contains("number = {8259}"), "{bib}");
        assert!(bib.contains("institution = {IETF}"), "{bib}");
        assert!(bib.contains("author = {Bray, T.}"), "{bib}");
        assert!(bib.contains("{JavaScript}"), "{bib}");
        assert!(bib.contains("{JSON}"), "{bib}");
        assert!(bib.contains("brayJavascriptObjectNotation2017"), "{bib}");
        assert!(
            bib.contains("url = {https://www.rfc-editor.org/info/rfc8259}"),
            "{bib}"
        );
        assert!(!bib.contains("url = {https://doi.org/"), "{bib}");
    }

    #[test]
    fn preprint_is_misc_not_a_techreport() {
        let bib = bib_of(json!({
            "datacite": {
                "doi": "10.48550/arxiv.2510.21368",
                "types": {"resourceTypeGeneral": "Preprint"},
                "titles": [{"title": "Efficient Exploration of Chemical Kinetics"}],
                "creators": [{"name": "Goswami, Rohit", "givenName": "Rohit", "familyName": "Goswami", "nameType": "Personal"}],
                "publicationYear": 2025,
                "publisher": "arXiv"
            }
        }));
        assert!(bib.starts_with("@misc{"), "{bib}");
        assert!(!bib.contains("@techreport"), "{bib}");
        assert!(bib.contains("howpublished = {arXiv:2510.21368}"), "{bib}");
    }

    #[test]
    fn software_entry_is_configurable_and_zenodo_records_are_labeled() {
        let concept = json!({
            "datacite": {
                "doi": "10.5281/zenodo.18753567",
                "types": {"resourceTypeGeneral": "Software"},
                "titles": [{"title": "readcon-core: a CON/convel reader/writer with FFI bindings"}],
                "creators": [{"name": "Goswami, Rohit", "givenName": "Rohit", "familyName": "Goswami", "nameType": "Personal"}],
                "publicationYear": 2026,
                "version": "0.16.2",
                "publisher": "Zenodo",
                "relatedIdentifiers": [{"relationType": "HasVersion", "relatedIdentifier": "10.5281/zenodo.23264985"}]
            }
        });
        let version = json!({
            "datacite": {
                "doi": "10.5281/zenodo.23264985",
                "types": {"resourceTypeGeneral": "Software"},
                "titles": [{"title": "readcon-core: a CON/convel reader/writer with FFI bindings"}],
                "creators": [{"name": "Goswami, Rohit", "givenName": "Rohit", "familyName": "Goswami", "nameType": "Personal"}],
                "publicationYear": 2026,
                "version": "0.16.2",
                "publisher": "Zenodo",
                "relatedIdentifiers": [{"relationType": "IsVersionOf", "relatedIdentifier": "10.5281/zenodo.18753567"}]
            }
        });
        let concept_bib = bib_of(concept);
        assert!(concept_bib.starts_with("@software{"), "{concept_bib}");
        assert!(
            concept_bib.contains("note = {Zenodo concept DOI"),
            "{concept_bib}"
        );
        assert!(
            concept_bib.contains("doi = {10.5281/zenodo.18753567}"),
            "{concept_bib}"
        );
        assert!(!concept_bib.contains("\n  version = "), "{concept_bib}");
        assert!(concept_bib.contains("{CON}"), "{concept_bib}");
        assert!(concept_bib.contains("{FFI}"), "{concept_bib}");

        let version_bib = bib_of(version);
        assert!(
            version_bib.contains("note = {Zenodo version DOI; concept 10.5281/zenodo.18753567}"),
            "{version_bib}"
        );
        assert!(version_bib.contains("version = {0.16.2}"), "{version_bib}");

        let stored = json!({"doi": "10.5281/zenodo.18753567"});
        let provider = json!({
            "datacite": {
                "doi": "10.5281/zenodo.18753567",
                "types": {"resourceTypeGeneral": "Software"},
                "titles": [{"title": "readcon-core"}],
                "creators": [{"givenName": "Rohit", "familyName": "Goswami", "nameType": "Personal"}],
                "publicationYear": 2026,
                "publisher": "Zenodo",
                "relatedIdentifiers": [{"relationType": "HasVersion", "relatedIdentifier": "10.5281/zenodo.1"}]
            }
        });
        let misc = render_works(
            &[assemble(&stored, Some(&provider))],
            &BibOptions {
                software_as_misc: true,
                ..Default::default()
            },
        );
        assert!(misc.starts_with("@misc{"), "{misc}");
        assert!(misc.contains("howpublished = {Zenodo}"), "{misc}");
        assert!(misc.contains("note = {Software, all versions}"), "{misc}");
        assert!(!misc.contains("publisher"), "{misc}");
    }

    #[test]
    fn zenodo_logins_are_not_authors() {
        let bib = bib_of(json!({
            "datacite": {
                "doi": "10.5281/zenodo.18529495",
                "types": {"resourceTypeGeneral": "Software"},
                "titles": [{"title": "eOn"}],
                "creators": [
                    {"name": "Rohit Goswami", "familyName": "Rohit Goswami", "nameType": "Personal"},
                    {"name": "t-brink", "familyName": "t-brink", "nameType": "Personal"},
                    {"name": "satishskamath", "familyName": "satishskamath", "nameType": "Personal"}
                ],
                "publicationYear": 2026,
                "publisher": "Zenodo",
                "relatedIdentifiers": [{"relationType": "HasVersion", "relatedIdentifier": "10.5281/zenodo.1"}]
            }
        }));
        assert!(bib.contains("Goswami, Rohit"), "{bib}");
        assert!(!bib.contains("t-brink"), "{bib}");
        assert!(!bib.contains("satishskamath"), "{bib}");
        assert!(bib.contains("Zenodo concept DOI"), "{bib}");
    }

    #[test]
    fn organisation_author_is_a_braced_literal() {
        let stored = json!({
            "title": "IEEE Standard for Floating-Point Arithmetic",
            "authors": [{"family": "{IEEE Computer Society}"}],
            "date": {"year": 2019},
            "doi": "10.1109/ieeestd.2019.8766229",
            "entry_type": "misc"
        });
        let bib = render_stored_entries(&[stored]);
        let author = bib.lines().find(|line| line.contains("author = ")).unwrap();
        assert!(author.contains("{{IEEE Computer Society}}"), "{author}");
        assert!(!author.contains("}}, {"), "{author}");
        assert!(
            bib.contains("ieeecomputersocietyIeeeStandardFloatingpoint2019"),
            "{bib}"
        );
    }

    #[test]
    fn custom_citation_key_is_kept() {
        let stored = json!({
            "title": "A paper",
            "authors": [{"family": "Goswami", "given": "Rohit"}],
            "date": {"year": 2026},
            "doi": "10.1/custom",
            "citation_key": "myCustomKey",
            "entry_type": "article",
            "journal": "MethodsX"
        });
        let first = render_stored_entries(std::slice::from_ref(&stored));
        let second = render_stored_entries(&[stored]);
        assert_eq!(first, second);
        assert!(first.contains("@article{myCustomKey,"), "{first}");
    }

    #[test]
    fn caller_authors_win_when_they_were_edited() {
        let stored = json!({
            "doi": "10.3389/fchem.2026.1807063",
            "authors": [{"family": "Goswami", "given": "Rohit"}],
            "user_fields": ["authors"]
        });
        let provider = json!({
            "crossref": {
                "type": "journal-article",
                "title": ["Enhanced climbing image"],
                "container-title": ["Frontiers in Chemistry"],
                "DOI": "10.3389/fchem.2026.1807063",
                "author": [
                    {"given": "Rohit", "family": "Goswami"},
                    {"given": "Miha", "family": "Gunde"}
                ],
                "issued": {"date-parts": [[2026]]}
            }
        });
        let bib = render_works(
            &[assemble(&stored, Some(&provider))],
            &BibOptions::default(),
        );
        assert!(!bib.contains("Gunde"), "{bib}");
        assert!(bib.contains("Goswami, Rohit"), "{bib}");
    }

    #[test]
    fn entities_and_title_markup_are_cleaned() {
        let bib = bib_of(json!({
            "crossref": {
                "type": "journal-article",
                "title": ["P\n                    <scp>ACKMOL</scp>\n                    : A package for building initial configurations"],
                "container-title": ["Computing in Science &amp; Engineering"],
                "DOI": "10.1002/jcc.21224",
                "author": [{"given": "S.R.", "family": "Bahn"}, {"given": "K.W.", "family": "Jacobsen"}]
            }
        }));
        assert!(
            bib.contains("title = {{PACKMOL}: A package for building initial configurations}"),
            "{bib}"
        );
        assert!(
            bib.contains(r"journal = {Computing in Science \& Engineering}"),
            "{bib}"
        );
        assert!(
            bib.contains("author = {Bahn, S. R. and Jacobsen, K. W.}"),
            "{bib}"
        );

        let bib = bib_of(json!({
            "crossref": {
                "type": "journal-article",
                "title": ["<tt>metatensor</tt>\n   and\n   <tt>metatomic</tt>\n   : Foundational libraries"],
                "DOI": "10.1063/5.0304911"
            }
        }));
        assert!(
            bib.contains("title = {{metatensor} and {metatomic}: Foundational libraries}"),
            "{bib}"
        );

        let bib = bib_of(json!({
            "crossref": {
                "type": "journal-article",
                "title": ["Efficient iterative schemes for\n   <i>ab initio</i>\n   total-energy calculations"],
                "DOI": "10.1103/physrevb.54.11169"
            }
        }));
        assert!(
            bib.contains(
                "title = {Efficient iterative schemes for ab initio total-energy calculations}"
            ),
            "{bib}"
        );
    }

    #[test]
    fn accented_initials_are_tex_accents() {
        assert_eq!(tex_initial_letters("Ádám"), "{\\'A}dám");
        assert_eq!(tex_initial_letters("Jean-Éric"), "Jean-{\\'E}ric");
        assert_eq!(tex_initial_letters("Vilhjálmur"), "Vilhjálmur");
        assert_eq!(tex_initial_letters("Ö. R."), "{\\\"O}. R.");
    }

    #[test]
    fn dashes_and_spaces_are_normalised() {
        assert_eq!(clean_name("Michaud\u{2010}Agrawal"), "Michaud-Agrawal");
        assert_eq!(
            clean_text("LAMMPS - a flexible tool"),
            "LAMMPS -- a flexible tool"
        );
        assert_eq!(
            clean_text("environment\u{2014}a library, OVITO\u{2013}the tool"),
            "environment---a library, OVITO--the tool"
        );
        assert_eq!(clean_text("Michaud\u{2010}Agrawal"), "Michaud-Agrawal");
        assert_eq!(
            clean_text("implementation  - PLDI '90"),
            "implementation -- PLDI '90"
        );
        assert_eq!(clean_text("a ( b ) , c &#x2013; d"), "a (b), c -- d");
        assert_eq!(clean_text("R&D & more"), "R&D & more");
    }

    #[test]
    fn title_case_is_left_to_the_style_and_names_stay_braced() {
        assert_eq!(
            display_title("Nudged Elastic Band Method Using Energy-Weighted Springs with Gaussian Process Regression"),
            "Nudged Elastic Band Method Using Energy-Weighted Springs with {Gaussian} Process Regression"
        );
        assert_eq!(
            display_title("Reaction-rate theory: fifty years after Kramers"),
            "Reaction-rate theory: fifty years after {Kramers}"
        );
        assert_eq!(
            display_title(
                "Python Materials Genomics (pymatgen): A robust, open-source python library"
            ),
            "Python {Materials} {Genomics} (pymatgen): A robust, open-source {Python} library"
        );
        assert_eq!(
            display_title("IEEE Standard for Floating-Point Arithmetic"),
            "{IEEE} Standard for Floating-Point Arithmetic"
        );
        assert_eq!(
            display_title("Updating quasi-Newton matrices with limited storage"),
            "Updating {quasi-Newton} matrices with limited storage"
        );
        // Caller braces are copied unchanged.
        assert_eq!(
            display_title("The {JavaScript Object Notation (JSON)} Data Interchange Format"),
            "The {JavaScript Object Notation (JSON)} Data Interchange Format"
        );
    }

    #[test]
    fn article_number_can_go_to_pages_or_note() {
        let provider = json!({
            "crossref": {
                "type": "journal-article",
                "title": ["Optimization methods for finding minimum energy paths"],
                "container-title": ["The Journal of Chemical Physics"],
                "volume": "128",
                "issue": "13",
                "article-number": "134106",
                "DOI": "10.1063/1.2841941"
            }
        });
        let work = assemble(&json!({"doi": "10.1063/1.2841941"}), Some(&provider));
        let render = |eid_as| {
            render_works(
                std::slice::from_ref(&work),
                &BibOptions {
                    eid_as,
                    ..Default::default()
                },
            )
        };
        let eid = render(EidPlacement::Eid);
        assert!(
            eid.contains("eid = {134106}") && !eid.contains("pages"),
            "{eid}"
        );
        let pages = render(EidPlacement::Pages);
        assert!(
            pages.contains("pages = {134106}") && !pages.contains("eid ="),
            "{pages}"
        );
        let note = render(EidPlacement::Note);
        assert!(
            note.contains("note = {Article 134106}") && !note.contains("eid ="),
            "{note}"
        );
        assert_eq!(EidPlacement::parse("PAGES"), EidPlacement::Pages);
        assert_eq!(EidPlacement::parse("anything"), EidPlacement::Eid);
    }

    #[test]
    fn local_clear_removes_provider_fields() {
        use crate::overrides::CollectionOverrides;
        use crate::tool_args::UpdateEntryMetadataArgs;
        let provider = json!({
            "crossref": {
                "type": "journal-article",
                "title": ["Packmol"],
                "author": [{"given": "L.", "family": "Martinez"}],
                "container-title": ["Journal of Computational Chemistry"],
                "issued": {"date-parts": [[2009, 3]]},
                "volume": "30",
                "DOI": "10.1002/jcc.21224",
                "URL": "https://example.org/landing"
            }
        });
        let mut store = CollectionOverrides::new("paper");
        store.record(
            "doi:10.1002/jcc.21224".into(),
            &UpdateEntryMetadataArgs {
                collection: "paper".into(),
                entry_id: "e1".into(),
                clear: Some(vec!["year".into(), "volume".into(), "url".into()]),
                ..Default::default()
            },
        );
        let mut entries = vec![
            json!({"id": "e1", "metadata": {"doi": "10.1002/jcc.21224", "date": {"year": 2009}}}),
        ];
        store.apply_to_entries(&mut entries);
        let bib = render_works(
            &[assemble(&entries[0]["metadata"], Some(&provider))],
            &BibOptions::default(),
        );
        assert!(bib.contains("journal = {"), "{bib}");
        assert!(!bib.contains("year ="), "{bib}");
        assert!(!bib.contains("volume ="), "{bib}");
        assert!(!bib.contains("url ="), "{bib}");
    }

    #[test]
    fn local_override_wins_over_the_provider_record() {
        use crate::overrides::{apply, EntryOverride};
        let provider = json!({
            "crossref": {
                "type": "report",
                "title": ["The JavaScript Object Notation (JSON) Data Interchange Format"],
                "author": [{"given": "Tim", "family": "Bray"}],
                "editor": [{"given": "T.", "family": "Bray"}],
                "publisher": "RFC Editor",
                "DOI": "10.17487/rfc8259",
                "URL": "https://www.rfc-editor.org/info/rfc8259"
            }
        });
        let mut ov = EntryOverride::default();
        ov.fields.insert("url".into(), None);
        ov.fields
            .insert("citation_key".into(), Some("brayJson2017".into()));
        ov.fields
            .insert("entry_type".into(), Some("techreport".into()));
        ov.editor = Some(Vec::new());
        let stored = apply(&json!({"doi": "10.17487/rfc8259"}), &ov);
        let bib = render_works(
            &[assemble(&stored, Some(&provider))],
            &BibOptions::default(),
        );
        assert!(bib.starts_with("@techreport{brayJson2017,"), "{bib}");
        assert!(!bib.contains("editor"), "{bib}");
        assert!(!bib.contains("url ="), "{bib}");
        assert!(bib.contains("number = {8259}"), "{bib}");

        let provider = json!({
            "crossref": {
                "type": "journal-article",
                "title": ["Software citation principles"],
                "author": [{"given": "Arfon M.", "family": "Smith"}, {"name": "FORCE11 Software Citation Working Group"}],
                "DOI": "10.7717/peerj-cs.86"
            }
        });
        let ov = EntryOverride {
            authors: Some(vec![
                json!({"family": "Smith", "given": "Arfon M."}),
                json!({"family": "FORCE11 Software Citation Working Group", "literal": true}),
            ]),
            ..Default::default()
        };
        let stored = apply(&json!({"doi": "10.7717/peerj-cs.86"}), &ov);
        let bib = render_works(
            &[assemble(&stored, Some(&provider))],
            &BibOptions::default(),
        );
        assert!(
            bib.contains(
                "author = {Smith, Arfon M. and {FORCE11 Software Citation Working Group}}"
            ),
            "{bib}"
        );
    }

    #[test]
    fn readcon_fixture_renders_clean_text() {
        let providers: HashMap<String, Value> =
            serde_json::from_str(include_str!("../tests/fixtures/providers.json")).unwrap();
        let mut dois: Vec<&String> = providers.keys().collect();
        dois.sort();
        let works: Vec<Work> = dois
            .iter()
            .map(|doi| assemble(&json!({"doi": doi}), Some(&providers[*doi])))
            .collect();
        let options = BibOptions {
            software_as_misc: true,
            eid_as: EidPlacement::Pages,
        };
        let exported = render_works(&works, &options);
        for entry in split_entries(&exported) {
            for line in entry.lines().skip(1) {
                let line = line.trim();
                if line == "}" || line.is_empty() {
                    continue;
                }
                assert!(
                    line.contains(" = {"),
                    "field broken over lines: {line}\n{entry}"
                );
                assert!(!line.contains("&amp;"), "{entry}");
                assert!(!line.contains('<'), "{entry}");
                assert!(!line.contains("  "), "{entry}");
                assert!(!line.contains(" - "), "{entry}");
                assert!(!line.starts_with("eid ="), "{entry}");
            }
            if entry.contains("howpublished = {Zenodo}") {
                assert!(entry.contains("note = {Software, "), "{entry}");
            }
        }
    }

    #[test]
    fn round_trip_of_the_readcon_bibliography() {
        let bib_src = include_str!("../tests/fixtures/readcon-cpc.bib");
        let providers: HashMap<String, Value> =
            serde_json::from_str(include_str!("../tests/fixtures/providers.json")).unwrap();
        let mut dois = Vec::new();
        for line in bib_src.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("doi = {") {
                let doi = rest.trim_end_matches(',').trim_end_matches('}').trim();
                if !doi.is_empty() {
                    dois.push(doi.to_string());
                }
            }
        }
        assert_eq!(dois.len(), 102);
        let works: Vec<Work> = dois
            .iter()
            .map(|doi| {
                let provider = providers
                    .get(doi)
                    .unwrap_or_else(|| panic!("missing {doi}"));
                assemble(&json!({"doi": doi}), Some(provider))
            })
            .collect();
        let exported = render_works(&works, &BibOptions::default());
        let entries = split_entries(&exported);
        assert_eq!(entries.len(), 102, "one entry per DOI");
        for (doi, work) in dois.iter().zip(works.iter()) {
            let entry = entries
                .iter()
                .find(|entry| {
                    entry
                        .to_ascii_lowercase()
                        .contains(&format!("doi = {{{doi}}}").to_ascii_lowercase())
                })
                .unwrap_or_else(|| panic!("missing entry for {doi}\n{exported}"));
            let provider = &providers[doi];
            if let Some(message) = provider.get("crossref") {
                let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "journal-article" => {
                        assert!(entry.starts_with("@article{"), "{doi}: {entry}");
                        assert!(entry.contains("journal = {"), "{doi}");
                    }
                    "proceedings-article" => {
                        assert!(entry.starts_with("@inproceedings{"), "{doi}: {entry}");
                        assert!(entry.contains("booktitle = {"), "{doi}: {entry}");
                        assert!(!entry.contains("\n  journal = "), "{doi}: {entry}");
                        let publisher = message
                            .get("publisher")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !publisher.is_empty() {
                            assert!(
                                !entry.contains(&format!("booktitle = {{{publisher}}}")),
                                "{doi}"
                            );
                        }
                    }
                    "book-chapter" => {
                        assert!(entry.starts_with("@incollection{"), "{doi}: {entry}");
                        assert!(entry.contains("booktitle = {"), "{doi}");
                        assert!(entry.contains("chapter = {"), "{doi}: {entry}");
                        assert!(entry.contains("pages = {"), "{doi}: {entry}");
                    }
                    "report" => {
                        assert!(entry.starts_with("@techreport{"), "{doi}: {entry}");
                        if doi.to_ascii_lowercase().contains("rfc") {
                            assert!(entry.contains("institution = {IETF}"), "{doi}: {entry}");
                            assert!(entry.contains("number = {"), "{doi}: {entry}");
                        }
                    }
                    "standard" => {
                        assert!(entry.starts_with("@misc{"), "{doi}: {entry}");
                        let author = entry
                            .lines()
                            .find(|line| line.contains("author = "))
                            .unwrap();
                        assert!(author.contains("{{") && author.contains("}}"), "{author}");
                        assert!(!author.contains("}}, {"), "{author}");
                    }
                    other => panic!("unexpected crossref type {other} for {doi}"),
                }
                if let Some(issue) = message.get("issue").and_then(Value::as_str) {
                    assert!(
                        entry.contains(&format!("number = {{{issue}}}")),
                        "{doi}: {entry}"
                    );
                }
                let page = message.get("page");
                if page.is_none() || page.is_some_and(Value::is_null) {
                    if let Some(article) = message.get("article-number").and_then(Value::as_str) {
                        assert!(
                            entry.contains(&format!("eid = {{{article}}}")),
                            "{doi}: {entry}"
                        );
                        assert!(!entry.contains(&format!("{article}--{article}")), "{doi}");
                    }
                }
            } else if let Some(attrs) = provider.get("datacite") {
                let general = attrs
                    .pointer("/types/resourceTypeGeneral")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if general == "Software" {
                    assert!(entry.starts_with("@software{"), "{doi}: {entry}");
                    if is_zenodo_doi(doi) {
                        assert!(
                            entry.contains("Zenodo concept DOI")
                                || entry.contains("Zenodo version DOI"),
                            "{doi}: {entry}"
                        );
                    }
                } else if general == "Preprint" {
                    assert!(entry.starts_with("@misc{"), "{doi}: {entry}");
                    assert!(entry.contains("howpublished = {arXiv:"), "{doi}: {entry}");
                }
            }
            assert!(!entry.contains("url = {https://doi.org/"), "{doi}: {entry}");
            assert!(!entry.contains("url = {http://doi.org/"), "{doi}: {entry}");
            let _ = work;
        }
        let again = render_works(
            &dois
                .iter()
                .map(|doi| assemble(&json!({"doi": doi}), Some(&providers[doi])))
                .collect::<Vec<_>>(),
            &BibOptions::default(),
        );
        assert_eq!(exported, again, "keys and fields stay stable");
    }

    fn split_entries(bib: &str) -> Vec<String> {
        bib.split("\n@")
            .enumerate()
            .map(|(index, part)| {
                if index == 0 {
                    part.to_string()
                } else {
                    format!("@{part}")
                }
            })
            .filter(|entry| entry.trim_start().starts_with('@'))
            .collect()
    }
}
