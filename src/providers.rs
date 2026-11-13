//! Crossref, DataCite, Zenodo, and the DOI handle API.
//!
//! [`ProviderBases::from_env`] is read once when the server starts.
//! `OOKCITE_PROVIDERS=off` leaves the set empty, so a lookup talks only
//! to the OokCite API. Each base variable replaces one production host.

use url::Url;

pub const CROSSREF: &str = "https://api.crossref.org";
pub const DATACITE: &str = "https://api.datacite.org";
pub const ZENODO: &str = "https://zenodo.org/api";
pub const HANDLES: &str = "https://doi.org/api";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderBases {
    pub crossref: String,
    pub datacite: String,
    pub zenodo: String,
    pub handles: String,
}

impl ProviderBases {
    pub fn production() -> Self {
        Self {
            crossref: CROSSREF.into(),
            datacite: DATACITE.into(),
            zenodo: ZENODO.into(),
            handles: HANDLES.into(),
        }
    }

    #[cfg(test)]
    pub fn shared(base: impl Into<String>) -> Self {
        let base = trim_base(&base.into());
        Self {
            crossref: base.clone(),
            datacite: base.clone(),
            zenodo: base.clone(),
            handles: base,
        }
    }

    /// Live bases, or `None` when providers are switched off.
    ///
    /// `off`, `none`, `disabled`, `0`, `false`, and `no` all switch them
    /// off. An empty value, or any other value, keeps them on. A base
    /// variable is ignored while they are off.
    pub fn from_env() -> Option<Self> {
        if !providers_enabled() {
            return None;
        }
        let mut bases = Self::production();
        apply_override(&mut bases.crossref, "OOKCITE_CROSSREF_API");
        apply_override(&mut bases.datacite, "OOKCITE_DATACITE_API");
        apply_override(&mut bases.zenodo, "OOKCITE_ZENODO_API");
        apply_override(&mut bases.handles, "OOKCITE_DOI_API");
        Some(bases)
    }

    pub fn is_production(&self) -> bool {
        self == &Self::production()
    }
}

pub fn providers_enabled() -> bool {
    match std::env::var("OOKCITE_PROVIDERS") {
        Ok(raw) => {
            let value = raw.trim().to_ascii_lowercase();
            if value.is_empty() {
                true
            } else {
                !matches!(
                    value.as_str(),
                    "off" | "none" | "disabled" | "0" | "false" | "no"
                )
            }
        }
        Err(_) => true,
    }
}

/// One line for `doctor`. Userinfo is dropped so a base URL cannot echo a
/// password into the report.
pub fn summary_line() -> String {
    match ProviderBases::from_env() {
        None => "providers: off (Crossref, DataCite, Zenodo, and doi.org are not called)".into(),
        Some(bases) if bases.is_production() => {
            "providers: live (Crossref, DataCite, Zenodo, doi.org)".into()
        }
        Some(bases) => format!(
            "providers: crossref {c}; datacite {d}; zenodo {z}; doi {h}",
            c = display_base(&bases.crossref),
            d = display_base(&bases.datacite),
            z = display_base(&bases.zenodo),
            h = display_base(&bases.handles),
        ),
    }
}

fn apply_override(slot: &mut String, key: &str) {
    let Ok(raw) = std::env::var(key) else {
        return;
    };
    let value = trim_base(&raw);
    if !value.is_empty() {
        *slot = value;
    }
}

fn trim_base(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

pub fn display_base(base: &str) -> String {
    let Ok(mut url) = Url::parse(base) else {
        return trim_base(base);
    };
    if url.password().is_some() || !url.username().is_empty() {
        let _ = url.set_password(None);
        let _ = url.set_username("");
    }
    trim_base(url.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_base_drops_userinfo() {
        let shown = display_base("http://user:secret@127.0.0.1:9/crossref/");
        assert_eq!(shown, "http://127.0.0.1:9/crossref");
        assert!(!shown.contains("secret"));
        assert!(!shown.contains("user"));
    }
}
