//! Model discovery — use a PAYG key without enumerating the catalog.
//!
//! WHY THIS EXISTS
//!
//! USER: *"ability to use a PAYG token with or without free tier without
//! having to enumerate all the models available from a hyperscaler/
//! router"*.
//!
//! `ModelsSpec` is Empty | List | Map, and every consumer downstream —
//! the proxy's `/v1/models`, the desktop picker via `/api/config`,
//! `Agent::set_model` — could only ever show what somebody typed by
//! hand. That is fine for a plan with six models on it. It is hopeless
//! for a router: this machine's own config hand-lists **23** OpenRouter
//! models out of the several hundred it serves, and the catalog moves
//! every week.
//!
//! THE HERMES LESSON, WHICH IS THE WHOLE DESIGN
//!
//! A live probe that REPLACES a curated list hides new models until the
//! catalog catches up — deepseek-v4-pro-0813 was live and billable for
//! days before any `/models` admitted it existed. So discovery here
//! MERGES and never replaces:
//!
//!   - a curated entry DECORATES a discovered name (its
//!     `context_length` wins), it never hides one;
//!   - a discovered name absent from the curated list still appears;
//!   - a name in NEITHER is still settable, because
//!     `Agent::set_model`'s last-resort arm already accepts an unlisted
//!     id and reports `UNVERIFIED`. Probe-by-id is how an undated
//!     catalog gets used before it catches up; do not make that
//!     stricter.
//!
//! BEST-EFFORT, ALWAYS. Discovery never blocks boot and never fails a
//! caller. A fetch that dies returns the curated list and the caller
//! says "discovery failed, showing configured models" — an empty picker
//! is worse than a stale one.

use crate::config::ProviderEntry;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a fetched catalog stays good. Long enough that a picker
/// opened twice in a minute costs one round trip, short enough that a
/// model added this morning shows up this afternoon.
const TTL: Duration = Duration::from_secs(15 * 60);

/// One model as the endpoint described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    /// Only some endpoints report this. OpenRouter does (`context_length`);
    /// the plain OpenAI-shaped ones do not.
    pub context_length: Option<u64>,
}

/// A model as the pickers should see it: the name, where its context
/// length came from, and whether anyone curated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub context_length: Option<u64>,
    /// True when this name came from the live catalog rather than the
    /// config. Surfaced so a UI can say "live" vs "configured".
    pub discovered: bool,
}

// ── the three-way rule ───────────────────────────────────────────────

/// Should this provider be discovered at all?
///
///   `discover = true`            → curated ∪ discovered.
///   absent AND `models` empty    → discover everything. Nothing curated
///                                  cannot mean "no models available";
///                                  it means nobody wrote the list yet.
///   `discover = false`           → curated only, never touch the
///                                  network. The escape hatch for an
///                                  air-gapped or metered endpoint.
pub fn wanted(entry: &ProviderEntry) -> bool {
    match entry.discover {
        Some(v) => v,
        None => entry.models.is_empty(),
    }
}

// ── cache ────────────────────────────────────────────────────────────
//
// Process-global, keyed by provider name, same shape as acl.rs's
// SESSION_PRINCIPALS. It exists so `set_model` — which is synchronous
// and must stay that way — can consult discovery without ever making a
// network call on the turn path.

type CacheMap = HashMap<String, (Instant, Vec<DiscoveredModel>)>;

fn cache() -> &'static Mutex<CacheMap> {
    static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached catalog for a provider, if one was fetched recently.
/// `None` means "nothing warm" — never "no models".
pub fn cached(provider: &str) -> Option<Vec<DiscoveredModel>> {
    let c = cache().lock().ok()?;
    let (at, models) = c.get(provider)?;
    if at.elapsed() > TTL {
        return None;
    }
    Some(models.clone())
}

/// Seed the cache directly. Used by the tests, and by any caller that
/// has a catalog in hand from somewhere other than a fetch.
pub fn put_cached(provider: &str, models: Vec<DiscoveredModel>) {
    if let Ok(mut c) = cache().lock() {
        c.insert(provider.to_string(), (Instant::now(), models));
    }
}

// ── fetch ────────────────────────────────────────────────────────────

/// Fetch `{base_url}/models`, cache it, return it.
///
/// Returns an EMPTY vec on any failure — the caller merges that with the
/// curated list and is left exactly where it started. There is no error
/// type on purpose: no consumer of this has anything useful to do with
/// one, and every consumer has something useful to do with "the curated
/// list, unchanged".
pub async fn models_for(entry: &ProviderEntry, home: &Path) -> Vec<DiscoveredModel> {
    if !wanted(entry) {
        return Vec::new();
    }
    if let Some(hit) = cached(&entry.name) {
        return hit;
    }
    if entry.base_url.is_empty() {
        return Vec::new();
    }
    let url = format!("{}/models", entry.base_url.trim_end_matches('/'));
    // The SAME key resolution every request uses — env var, then
    // $VELES_HOME/.env. Never re-derive this; providers::resolve_api_key
    // is the one place keys are read and it must stay that way.
    let key = crate::providers::resolve_api_key(entry, home);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("discovery: no http client for {}: {e}", entry.name);
            return Vec::new();
        }
    };
    let mut req = client.get(&url);
    if !key.is_empty() {
        req = req.bearer_auth(&key);
    }
    let body = match req.send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => {
            tracing::debug!("discovery: {} returned HTTP {}", entry.name, r.status());
            return Vec::new();
        }
        Err(e) => {
            tracing::debug!("discovery: {} unreachable: {e}", entry.name);
            return Vec::new();
        }
    };
    let models = parse_catalog(&body);
    if models.is_empty() {
        // A 200 with nothing usable in it is a failure too — do not
        // cache it, or one bad answer poisons the picker for 15 minutes.
        tracing::debug!("discovery: {} returned no parseable models", entry.name);
        return Vec::new();
    }
    put_cached(&entry.name, models.clone());
    models
}

/// Pull models out of an OpenAI-compatible `/models` body.
///
/// Split out from the fetch so it is testable without a socket, which is
/// the only reason every field-shape below is pinned by a test rather
/// than by hope.
pub fn parse_catalog(body: &str) -> Vec<DiscoveredModel> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // `{"data":[...]}` is the OpenAI shape everything here speaks; a
    // bare array is accepted too, because some routers answer that way
    // and refusing it would be pedantry with no upside.
    let rows = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a.clone(),
        None => match v.as_array() {
            Some(a) => a.clone(),
            None => return Vec::new(),
        },
    };
    let mut out = Vec::new();
    for row in rows {
        let id = match row.get("id").and_then(|i| i.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        // OpenRouter reports `context_length`; some report it nested
        // under `top_provider`. Anything else simply has none, and the
        // merge falls back to the curated or provider-level value.
        let context_length = row
            .get("context_length")
            .and_then(|c| c.as_u64())
            .or_else(|| {
                row.get("top_provider")
                    .and_then(|t| t.get("context_length"))
                    .and_then(|c| c.as_u64())
            });
        out.push(DiscoveredModel { id, context_length });
    }
    out
}

// ── merge ────────────────────────────────────────────────────────────

/// Curated ∪ discovered, sorted, deduplicated.
///
/// PRECEDENCE for context_length, in this order and no other:
///   1. the curated `models` map entry — an operator typed it;
///   2. what the endpoint reported;
///   3. the provider-level `context_length` default.
///
/// A curated override losing to a discovered default is the bug this
/// ordering exists to prevent: the whole reason someone writes
/// `context_length` into their config is that the endpoint's number is
/// wrong for their plan.
pub fn merge(entry: &ProviderEntry, discovered: &[DiscoveredModel]) -> Vec<ModelInfo> {
    let mut by_id: HashMap<String, ModelInfo> = HashMap::new();
    for d in discovered {
        by_id.insert(
            d.id.clone(),
            ModelInfo {
                id: d.id.clone(),
                context_length: d.context_length.or(entry.context_length),
                discovered: true,
            },
        );
    }
    for name in entry.models.model_names() {
        let curated_len = entry.models.context_length(&name);
        match by_id.get_mut(&name) {
            // Curated AND discovered: keep it listed as discovered (it
            // is live), but let the curated context_length win.
            Some(existing) => {
                if curated_len.is_some() {
                    existing.context_length = curated_len;
                }
            }
            None => {
                by_id.insert(
                    name.clone(),
                    ModelInfo {
                        id: name,
                        context_length: curated_len.or(entry.context_length),
                        discovered: false,
                    },
                );
            }
        }
    }
    let mut out: Vec<ModelInfo> = by_id.into_values().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The merged list using only what is already cached — no network, no
/// async. This is what `Agent::set_model` calls: a model-picker lookup
/// that blocks a turn on a router round-trip is exactly the failure this
/// feature exists to avoid.
pub fn merged_cached(entry: &ProviderEntry) -> Vec<ModelInfo> {
    let discovered = if wanted(entry) {
        cached(&entry.name).unwrap_or_default()
    } else {
        Vec::new()
    };
    merge(entry, &discovered)
}

/// Warm the cache for every provider that wants discovering. Best-effort
/// and concurrent; callers that want a fresh list before rendering (the
/// proxy's /v1/models, the desktop's /api/config) await this first.
pub async fn warm_all(cfg: &crate::config::VelesConfig) {
    let home = crate::config::config_dir(); // .env: XDG config
    let mut tasks = Vec::new();
    for entry in &cfg.custom_providers {
        if !wanted(entry) || cached(&entry.name).is_some() {
            continue;
        }
        let e = entry.clone();
        let h = home.clone();
        tasks.push(async move {
            let _ = models_for(&e, &h).await;
        });
    }
    futures::future::join_all(tasks).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelsSpec;
    use serde_yaml::Value as Y;

    fn map_entry(name: &str, pairs: &[(&str, u64)]) -> ProviderEntry {
        let mut m = std::collections::HashMap::new();
        for (k, len) in pairs {
            let mut inner = serde_yaml::Mapping::new();
            inner.insert(Y::from("context_length"), Y::from(*len));
            m.insert(k.to_string(), Y::Mapping(inner));
        }
        ProviderEntry {
            name: name.into(),
            models: ModelsSpec::Map(m),
            ..Default::default()
        }
    }

    fn disc(id: &str, len: Option<u64>) -> DiscoveredModel {
        DiscoveredModel {
            id: id.into(),
            context_length: len,
        }
    }

    // ── the three-way rule ───────────────────────────────────────────

    #[test]
    fn empty_models_means_discover_everything() {
        let e = ProviderEntry::default();
        assert!(wanted(&e), "nothing curated means nobody wrote the list");
    }

    #[test]
    fn curated_without_discover_stays_curated() {
        let e = map_entry("p", &[("a", 1)]);
        assert!(!wanted(&e));
    }

    #[test]
    fn discover_true_adds_to_a_curated_list() {
        let mut e = map_entry("p", &[("a", 1)]);
        e.discover = Some(true);
        assert!(wanted(&e), "this is the case that helps a router config");
    }

    #[test]
    fn discover_false_never_touches_the_network_even_when_empty() {
        let e = ProviderEntry {
            discover: Some(false),
            ..Default::default()
        };
        assert!(!wanted(&e));
    }

    // ── merge: the hermes lesson ─────────────────────────────────────

    #[test]
    fn a_discovered_model_absent_from_the_curated_list_still_appears() {
        let e = map_entry("p", &[("curated-only", 1000)]);
        let out = merge(&e, &[disc("brand-new-model", Some(200_000))]);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"brand-new-model"));
        assert!(ids.contains(&"curated-only"), "curation is not replaced");
    }

    #[test]
    fn a_curated_context_length_beats_the_discovered_one() {
        let e = map_entry("p", &[("shared", 1000)]);
        let out = merge(&e, &[disc("shared", Some(999_999))]);
        let m = out.iter().find(|m| m.id == "shared").unwrap();
        assert_eq!(
            m.context_length,
            Some(1000),
            "an operator typed 1000 because the endpoint's number is wrong for their plan"
        );
    }

    #[test]
    fn a_discovered_context_length_is_used_when_nothing_is_curated() {
        let e = ProviderEntry {
            name: "p".into(),
            ..Default::default()
        };
        let out = merge(&e, &[disc("m", Some(128_000))]);
        assert_eq!(out[0].context_length, Some(128_000));
    }

    #[test]
    fn the_provider_default_is_the_last_fallback() {
        let e = ProviderEntry {
            name: "p".into(),
            context_length: Some(32_000),
            ..Default::default()
        };
        let out = merge(&e, &[disc("m", None)]);
        assert_eq!(out[0].context_length, Some(32_000));
    }

    #[test]
    fn a_curated_and_discovered_model_is_marked_live() {
        let e = map_entry("p", &[("shared", 1000)]);
        let out = merge(&e, &[disc("shared", None)]);
        assert!(out[0].discovered, "it IS in the live catalog");
    }

    #[test]
    fn a_failed_fetch_leaves_the_curated_list_intact() {
        // models_for returns an empty vec on failure; merging that is
        // the identity on the curated list.
        let e = map_entry("p", &[("a", 1), ("b", 2)]);
        let out = merge(&e, &[]);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a", "b"],
            "an empty picker is worse than a stale one"
        );
    }

    #[test]
    fn merge_deduplicates_and_sorts() {
        let e = map_entry("p", &[("b", 1)]);
        let out = merge(&e, &[disc("b", None), disc("a", None)]);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn a_list_spec_merges_too_even_though_it_carries_no_lengths() {
        let e = ProviderEntry {
            name: "p".into(),
            models: ModelsSpec::List(vec!["glm-4.5".into()]),
            ..Default::default()
        };
        let out = merge(&e, &[disc("glm-5", Some(1))]);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["glm-4.5", "glm-5"]);
    }

    // ── parsing ──────────────────────────────────────────────────────

    #[test]
    fn parses_the_openai_data_shape() {
        let out = parse_catalog(r#"{"object":"list","data":[{"id":"gpt-x"},{"id":"gpt-y"}]}"#);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "gpt-x");
        assert_eq!(out[0].context_length, None);
    }

    #[test]
    fn parses_openrouters_context_length() {
        let out = parse_catalog(r#"{"data":[{"id":"a/b","context_length":200000}]}"#);
        assert_eq!(out[0].context_length, Some(200_000));
    }

    #[test]
    fn parses_a_nested_top_provider_context_length() {
        let out =
            parse_catalog(r#"{"data":[{"id":"a/b","top_provider":{"context_length":64000}}]}"#);
        assert_eq!(out[0].context_length, Some(64_000));
    }

    #[test]
    fn parses_a_bare_array() {
        let out = parse_catalog(r#"[{"id":"solo"}]"#);
        assert_eq!(out[0].id, "solo");
    }

    #[test]
    fn junk_is_no_models_rather_than_a_panic() {
        assert!(parse_catalog("not json").is_empty());
        assert!(parse_catalog("{}").is_empty());
        assert!(parse_catalog(r#"{"data":[{"no_id":1}]}"#).is_empty());
        assert!(parse_catalog(r#"{"data":[{"id":""}]}"#).is_empty());
    }

    // ── the cache ────────────────────────────────────────────────────

    #[test]
    fn merged_cached_reads_the_cache_without_a_network_call() {
        let mut e = map_entry("cache-test-provider", &[("curated", 5)]);
        e.discover = Some(true);
        put_cached("cache-test-provider", vec![disc("live", Some(9))]);
        let out = merged_cached(&e);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["curated", "live"]);
    }

    #[test]
    fn merged_cached_on_a_cold_cache_is_just_the_curated_list() {
        let mut e = map_entry("cold-provider", &[("curated", 5)]);
        e.discover = Some(true);
        let out = merged_cached(&e);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["curated"], "cold is not empty, it is uncurated");
    }

    #[test]
    fn a_discover_false_provider_ignores_even_a_warm_cache() {
        let mut e = map_entry("optout-provider", &[("curated", 5)]);
        e.discover = Some(false);
        put_cached("optout-provider", vec![disc("live", None)]);
        let out = merged_cached(&e);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["curated"], "opting out must mean opting out");
    }
}
