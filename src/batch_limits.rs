//! Upfront quota / membership preflight and identity-safe DOI TTL cache.
//!
//! Metered multi-lookup batches must refuse when they cannot fit remaining
//! daily quota, and prefer collection membership (free path) before fan-out.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::collection_entries::{entry_doi, looks_like_doi_token, normalize_doi_token};
use crate::constants::{ANON_BATCH_SOFT_CAP, DOI_CACHE_TTL_SECS, READ_ONLY_BATCH_CONCURRENCY};

/// Snapshot from `GET /api/v1/me` (subset used for preflight).
#[derive(Debug, Clone, Default)]
pub struct MeQuota {
    pub plan: String,
    pub lookups_remaining: Option<u32>,
    pub lookups_limit: Option<u32>,
}

impl MeQuota {
    pub fn from_json(v: &serde_json::Value) -> Self {
        Self {
            plan: v["plan"].as_str().unwrap_or("?").to_string(),
            lookups_remaining: v["lookups_remaining"]
                .as_u64()
                .or_else(|| v["lookups_remaining"].as_i64().map(|n| n.max(0) as u64))
                .map(|n| n as u32),
            lookups_limit: v["lookups_limit"]
                .as_u64()
                .or_else(|| v["lookups_limit"].as_i64().map(|n| n.max(0) as u64))
                .map(|n| n as u32),
        }
    }
}

/// Result of planning a multi-item metered batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchPreflight {
    /// Normalized DOIs (or opaque query keys) that still need a metered lookup.
    pub need_lookup: Vec<String>,
    /// Items answered from collection membership (normalized DOI -> optional title hint).
    pub members: Vec<(String, Option<String>)>,
    /// If set, refuse the whole batch and do not issue lookups.
    pub refuse_message: Option<String>,
}

/// Pure preflight: membership first, then quota on remaining metered work.
///
/// `items` are raw DOI or citation strings; only those that `looks_like_doi_token`
/// participate in membership matching. Non-DOI queries always need lookup.
///
/// `member_dois` holds normalized DOI tokens known to be in any user collection.
/// `member_titles` optional title by normalized DOI for agent-facing VALID lines.
pub fn plan_metered_batch(
    items: &[String],
    member_dois: &HashSet<String>,
    member_titles: &HashMap<String, String>,
    quota: Option<&MeQuota>,
    has_api_key: bool,
) -> BatchPreflight {
    let mut members = Vec::new();
    let mut need_lookup = Vec::new();

    for item in items {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        if looks_like_doi_token(trimmed) {
            let key = normalize_doi_token(trimmed);
            if member_dois.contains(&key) {
                let title = member_titles.get(&key).cloned();
                members.push((key, title));
                continue;
            }
            need_lookup.push(trimmed.to_string());
        } else {
            need_lookup.push(trimmed.to_string());
        }
    }

    let metered = need_lookup.len() as u32;

    if !has_api_key {
        if metered > ANON_BATCH_SOFT_CAP {
            return BatchPreflight {
                need_lookup: Vec::new(),
                members,
                refuse_message: Some(format!(
                    "REFUSED: anonymous / no OOKCITE_API_KEY session — refusing batch of {metered} \
                     metered lookups (soft cap {ANON_BATCH_SOFT_CAP}). Set OOKCITE_API_KEY for \
                     higher limits, shrink the batch, or import into a collection and re-verify \
                     members for free. IP daily limits still apply (~20/day anonymous)."
                )),
            };
        }
        return BatchPreflight {
            need_lookup,
            members,
            refuse_message: None,
        };
    }

    if let Some(q) = quota {
        if let Some(remaining) = q.lookups_remaining {
            if metered > remaining {
                let limit_hint = q
                    .lookups_limit
                    .map(|l| format!(" (plan {} limit {l}/day)", q.plan))
                    .unwrap_or_else(|| format!(" (plan {})", q.plan));
                let member_n = members.len();
                return BatchPreflight {
                    need_lookup: Vec::new(),
                    members,
                    refuse_message: Some(format!(
                        "REFUSED: batch needs {metered} metered lookup(s) but only {remaining} \
                         remaining{limit_hint}. {member_n} DOI(s) already in your collections \
                         would be free. Shrink the batch, upgrade plan, wait for daily reset, \
                         or import bibliography into a collection first so revisits are free."
                    )),
                };
            }
        }
        // remaining unknown: proceed (honest degrade) — server still meters.
    }

    BatchPreflight {
        need_lookup,
        members,
        refuse_message: None,
    }
}

/// Format collection-member lines for verify_references output.
pub fn format_member_valid_lines(members: &[(String, Option<String>)]) -> Vec<String> {
    members
        .iter()
        .map(|(doi, title)| {
            let t = title.as_deref().unwrap_or("(in your collection)");
            format!("VALID {doi} : {t} [collection — not metered]")
        })
        .collect()
}

/// Identity-safe process-local exact-DOI metadata cache (TTL).
#[derive(Clone, Default)]
pub struct DoiResponseCache {
    inner: Arc<Mutex<HashMap<String, (Instant, serde_json::Value)>>>,
    ttl: Duration,
}

impl DoiResponseCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_secs.max(60)),
        }
    }

    pub fn with_default_ttl() -> Self {
        Self::new(DOI_CACHE_TTL_SECS)
    }

    /// Return cached metadata only if embedded DOI normalizes to `key`.
    pub fn get_valid(&self, requested_doi: &str) -> Option<serde_json::Value> {
        let key = normalize_doi_token(requested_doi);
        let mut guard = self.inner.lock().ok()?;
        let (at, meta) = guard.get(&key)?.clone();
        if at.elapsed() > self.ttl {
            guard.remove(&key);
            return None;
        }
        if !metadata_doi_matches_json(&meta, &key) {
            guard.remove(&key);
            return None;
        }
        Some(meta)
    }

    /// Store only when identity matches the request key.
    pub fn put_if_identity_ok(&self, requested_doi: &str, meta: serde_json::Value) {
        let key = normalize_doi_token(requested_doi);
        if !metadata_doi_matches_json(&meta, &key) {
            return;
        }
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(key, (Instant::now(), meta));
        }
    }

    #[cfg(test)]
    pub fn len_for_test(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// Embedded `meta.doi` must normalize equal to `requested_key` (already normalized or not).
pub fn metadata_doi_matches_json(meta: &serde_json::Value, requested: &str) -> bool {
    let Some(d) = meta["doi"].as_str().filter(|s| !s.is_empty()) else {
        return false;
    };
    normalize_doi_token(d) == normalize_doi_token(requested)
}

/// Collect normalized DOIs + titles from a COLLECTION_GET-style JSON body.
pub fn collect_dois_from_collection_body(
    body: &serde_json::Value,
    dois: &mut HashSet<String>,
    titles: &mut HashMap<String, String>,
) {
    let entries = body["entries"]
        .as_array()
        .or_else(|| body["items"].as_array())
        .cloned()
        .unwrap_or_default();
    for entry in entries {
        if let Some(doi) = entry_doi(&entry) {
            let key = normalize_doi_token(&doi);
            dois.insert(key.clone());
            if let Some(t) = entry["metadata"]["title"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                titles.insert(key, t.to_string());
            }
        }
    }
}

pub fn read_only_concurrency() -> usize {
    std::env::var("OOKCITE_MCP_READ_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| (1..=64).contains(&n))
        .unwrap_or(READ_ONLY_BATCH_CONCURRENCY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_refuses_when_quota_too_low() {
        let items: Vec<String> = (0..5).map(|i| format!("10.1/test{i}")).collect();
        let q = MeQuota {
            plan: "free".into(),
            lookups_remaining: Some(2),
            lookups_limit: Some(60),
        };
        let pf = plan_metered_batch(&items, &HashSet::new(), &HashMap::new(), Some(&q), true);
        assert!(pf.refuse_message.is_some());
        assert!(pf.need_lookup.is_empty());
        assert!(pf.refuse_message.unwrap().contains("REFUSED"));
    }

    #[test]
    fn preflight_proceeds_when_quota_ok() {
        let items = vec!["10.1038/187493a0".into(), "10.1/x".into()];
        let q = MeQuota {
            plan: "academic".into(),
            lookups_remaining: Some(100),
            lookups_limit: Some(20_000),
        };
        let pf = plan_metered_batch(&items, &HashSet::new(), &HashMap::new(), Some(&q), true);
        assert!(pf.refuse_message.is_none());
        assert_eq!(pf.need_lookup.len(), 2);
    }

    #[test]
    fn membership_reduces_metered_count() {
        let items = vec!["10.1038/187493a0".into(), "10.1/need-lookup".into()];
        let mut members = HashSet::new();
        members.insert(normalize_doi_token("10.1038/187493a0"));
        let mut titles = HashMap::new();
        titles.insert(
            normalize_doi_token("10.1038/187493a0"),
            "Stimulated Optical Radiation in Ruby".into(),
        );
        let q = MeQuota {
            plan: "free".into(),
            lookups_remaining: Some(1),
            lookups_limit: Some(60),
        };
        // 1 member + 1 need_lookup; remaining 1 → proceed
        let pf = plan_metered_batch(&items, &members, &titles, Some(&q), true);
        assert!(pf.refuse_message.is_none());
        assert_eq!(pf.need_lookup.len(), 1);
        assert_eq!(pf.members.len(), 1);
        // If remaining were 0, would refuse
        let q0 = MeQuota {
            plan: "free".into(),
            lookups_remaining: Some(0),
            lookups_limit: Some(60),
        };
        let pf0 = plan_metered_batch(&items, &members, &titles, Some(&q0), true);
        assert!(pf0.refuse_message.is_some());
        assert!(pf0.need_lookup.is_empty());
        assert_eq!(pf0.members.len(), 1);
    }

    #[test]
    fn anon_soft_cap_refuses_large_batch() {
        let items: Vec<String> = (0..20).map(|i| format!("10.1/a{i}")).collect();
        let pf = plan_metered_batch(&items, &HashSet::new(), &HashMap::new(), None, false);
        assert!(pf.refuse_message.is_some());
        assert!(pf.need_lookup.is_empty());
    }

    #[test]
    fn cache_identity_gate() {
        let cache = DoiResponseCache::new(600);
        let good = serde_json::json!({"doi": "10.1038/187493a0", "title": "Ruby"});
        let bad = serde_json::json!({"doi": "10.1/other", "title": "Wrong"});
        cache.put_if_identity_ok("10.1038/187493a0", good.clone());
        cache.put_if_identity_ok("10.1038/187493a0", bad);
        let got = cache.get_valid("10.1038/187493a0").unwrap();
        assert_eq!(got["title"], "Ruby");
        assert_eq!(cache.len_for_test(), 1);
    }

    #[test]
    fn metadata_doi_match_normalizes() {
        let meta = serde_json::json!({"doi": "DOI:10.1038/187493A0"});
        assert!(metadata_doi_matches_json(&meta, "10.1038/187493a0"));
        assert!(!metadata_doi_matches_json(&meta, "10.1/x"));
    }

    #[test]
    fn read_only_concurrency_band() {
        let n = READ_ONLY_BATCH_CONCURRENCY;
        assert!(
            (16..=20).contains(&n),
            "default concurrency {n} not in 16..=20"
        );
    }
}
