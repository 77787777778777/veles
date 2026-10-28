//! `[slopdrop]` — the module's own config section, read from the
//! gateway's config.toml by the module (PLAN-plugins-installer.md R5:
//! core no longer types it; the module host hands the section over in
//! `ModuleContext::config`, and the module re-reads it from the file
//! when the file changes, so an operator's edit takes effect without a
//! reload, exactly as it did when core read it).

use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

/// `[slopdrop]` — evaluate Tcl in a slopdrop instance instead of in the
/// local throwaway sandbox.
///
/// veles' own evaluator is a temp file through `sandbox::execute`: safe,
/// and stateless. Every proc dies with the process, so the channel
/// cannot build anything. slopdrop is the other half of that — a
/// persistent, git-backed interpreter with a decade of accumulated
/// procs — and pointing `tcl` at it means one bot in the channel can
/// offer both, instead of two bots offering one each.
///
/// Unset (the default) leaves veles' OWN spellings exactly as they
/// were: `.tcl` and `/tcl` answer from the local sandbox, which is the
/// point of them. What it does change, since 2026-09-12, is the BARE
/// `tcl <code>` spelling on IRC — slopdrop's word, which veles answers
/// to only because it is holding slopdrop's interpreter. With none
/// configured it declines rather than answering out of an empty one,
/// because "invalid command name" from a stateless sandbox reads as
/// *your proc is gone* to a room whose procs are all still there.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SlopdropSection {
    /// Off unless asked for.
    #[serde(default)]
    pub enabled: bool,
    /// Base URL of slopdrop's HTTP API, e.g. `http://127.0.0.1:8080`.
    #[serde(default)]
    pub url: Option<String>,
    /// Env var (or `.env` key) holding the NON-admin bearer token. This
    /// is the one nearly every call uses: below-owner callers get
    /// slopdrop's ordinary `tcl`, which is what an IRC user has always
    /// had there.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Env var holding the ADMIN bearer token, used only for a caller
    /// veles resolves as owner. Unset means nobody gets it — which is
    /// the right default, and mirrors slopdrop's own posture of
    /// declining to privilege anyone it was not told about.
    #[serde(default)]
    pub admin_token_env: Option<String>,
    /// How long to wait. slopdrop's own eval timeout is 30s by default,
    /// so this wants to be a little longer than that.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// The local OUTPUT check for a PUBLIC `/tcl` reply: the sidecar
    /// scorer endpoints, tried in order (the VPS one first:
    /// `http://veles-classifier:8800`). When this is empty or no endpoint
    /// answers, the public reply fails CLOSED to link-only — the output
    /// check is the only thing that lets a composed/ad-hoc/stateful
    /// result inline, so "classifier off" must never mean "skip the
    /// check". Replaces the DeepSeek proc rating entirely.
    #[serde(default)]
    pub output_classifier: Vec<String>,
    /// Scorer threshold for the 7 toxicity heads (relaxed 0.05→0.30 on
    /// 2026-10-07: repetitive/spaced benign output scored 0.09–0.14 and
    /// false-positived; false negatives are acceptable, false positives
    /// are not). The deployed value lives in the VPS config.
    #[serde(default)]
    pub output_classifier_tox: Option<f64>,
    /// Scorer threshold for the 9 identity-mention heads (start 0.25).
    #[serde(default)]
    pub output_classifier_id: Option<f64>,
    /// Where SLOPDROP reaches this gateway's desktop API, for the Bluesky
    /// reads `bsky::query` makes as the bot (the module's open route
    /// `/api/plugins/slopdrop/open/bsky-read`, a per-evaluation token).
    /// On the VPS slopdrop runs on the host and the gateway publishes
    /// its API on the host's loopback: `http://127.0.0.1:8642`. Unset:
    /// no reads, and `bsky::query` says so.
    #[serde(default)]
    pub bsky_read_url: Option<String>,
}

/// The section in force, as the module last read it.
struct Current {
    section: Arc<SlopdropSection>,
    /// The config file's mtime the snapshot came from; `None` for a
    /// snapshot a test installed (never refreshed from the file).
    mtime: Option<std::time::SystemTime>,
    pinned: bool,
}

fn current() -> &'static RwLock<Current> {
    static C: OnceLock<RwLock<Current>> = OnceLock::new();
    C.get_or_init(|| {
        RwLock::new(Current {
            section: Arc::new(SlopdropSection::default()),
            mtime: None,
            pinned: false,
        })
    })
}

fn config_path() -> std::path::PathBuf {
    veles_core::config::config_dir().join("config.toml")
}

/// Parse the `[slopdrop]` table out of the raw config (the JSON shape
/// `pluginhost::raw_config_json` gives, which is also what the module
/// context carries).
pub fn section_from(raw: &serde_json::Value) -> SlopdropSection {
    raw.get("slopdrop")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Install `section` as the one in force (the module's `start`, from
/// its context). Later reads refresh from the file when it changes.
pub fn install(section: SlopdropSection) {
    let mtime = std::fs::metadata(config_path())
        .and_then(|m| m.modified())
        .ok();
    let mut c = current().write().unwrap_or_else(|p| p.into_inner());
    c.section = Arc::new(section);
    c.mtime = mtime;
    c.pinned = false;
}

/// Pin `section` for a test: never refreshed from any file.
pub fn pin_for_test(section: SlopdropSection) {
    let mut c = current().write().unwrap_or_else(|p| p.into_inner());
    c.section = Arc::new(section);
    c.mtime = None;
    c.pinned = true;
}

/// The section in force. Re-read from config.toml when the file's
/// mtime moved since the snapshot (one `stat` per call; the parse only
/// on a change), so a `/tcl` after an edit sees the edit.
pub fn settings() -> Arc<SlopdropSection> {
    let (pinned, mtime, cur) = {
        let c = current().read().unwrap_or_else(|p| p.into_inner());
        (c.pinned, c.mtime, c.section.clone())
    };
    if pinned {
        return cur;
    }
    let now = std::fs::metadata(config_path())
        .and_then(|m| m.modified())
        .ok();
    if now == mtime {
        return cur;
    }
    let fresh = Arc::new(section_from(&veles_core::pluginhost::raw_config_json()));
    let mut c = current().write().unwrap_or_else(|p| p.into_inner());
    c.section = fresh.clone();
    c.mtime = now;
    fresh
}

impl SlopdropSection {
    /// Is the bridge configured at all: on, and with a URL.
    pub fn is_enabled(&self) -> bool {
        self.enabled && self.url.as_deref().is_some_and(|u| !u.trim().is_empty())
    }
}

/// The knobs `veles config defaults|fill|check` document for this
/// section: every field, with its default, the way core's typed
/// sections are documented (`config_surface::knobs` walks a struct; this
/// walks ours).
pub fn config_surface() -> Vec<veles_core::config_surface::Knob> {
    let value = serde_json::to_value(SlopdropSection::default()).expect("the section serializes");
    let mut out = Vec::new();
    if let serde_json::Value::Object(m) = value {
        for (key, v) in m {
            let path = format!("slopdrop.{key}");
            let default = match &v {
                serde_json::Value::Null => "unset".to_string(),
                serde_json::Value::Array(a) if a.is_empty() => "[]".to_string(),
                other => other.to_string(),
            };
            let doc = match key.as_str() {
                "enabled" => {
                    "evaluate tcl/.tcl//tcl in a slopdrop instance instead of the local sandbox"
                }
                "url" => "base URL of slopdrop's HTTP API, e.g. http://127.0.0.1:8080",
                "token_env" => "env/.env key holding the NON-admin bearer token",
                "admin_token_env" => {
                    "env/.env key holding the ADMIN bearer token (owner callers only)"
                }
                "timeout_ms" => {
                    "how long to wait on the bridge; longer than slopdrop's own 30 s eval timeout"
                }
                "output_classifier" => {
                    "the public /tcl output scorer endpoints, tried in order; none → link-only"
                }
                "output_classifier_tox" => "scorer threshold for the toxicity heads (0.30)",
                "output_classifier_id" => "scorer threshold for the identity-mention heads (0.25)",
                _ => "",
            };
            out.push(veles_core::config_surface::Knob::blob_documented(
                &path, &default, doc,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_is_off_until_both_switches_are_set() {
        let mut s = SlopdropSection::default();
        assert!(!s.is_enabled(), "off by default");
        s.enabled = true;
        assert!(!s.is_enabled(), "enabled without a url is not a bridge");
        s.url = Some("  ".into());
        assert!(!s.is_enabled(), "a blank url is no url");
        s.url = Some("http://127.0.0.1:8080".into());
        assert!(s.is_enabled());
    }

    #[test]
    fn the_section_parses_out_of_the_raw_config() {
        let raw = serde_json::json!({"slopdrop": {"enabled": true, "url": "http://x:1", "output_classifier": ["http://c:2"]}, "model": {}});
        let s = section_from(&raw);
        assert!(s.enabled);
        assert_eq!(s.url.as_deref(), Some("http://x:1"));
        assert_eq!(s.output_classifier, vec!["http://c:2".to_string()]);
        assert_eq!(
            section_from(&serde_json::json!({})),
            SlopdropSection::default()
        );
    }

    #[test]
    fn every_field_is_on_the_config_surface() {
        let paths: Vec<String> = config_surface().into_iter().map(|k| k.path).collect();
        for key in [
            "enabled",
            "url",
            "token_env",
            "admin_token_env",
            "timeout_ms",
            "output_classifier",
            "output_classifier_tox",
            "output_classifier_id",
        ] {
            assert!(
                paths.contains(&format!("slopdrop.{key}")),
                "{key} missing from {paths:?}"
            );
        }
    }
}
