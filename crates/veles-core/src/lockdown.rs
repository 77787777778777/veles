//! Lockdown: the connectors go quiet and only the local frontends act.
//!
//! USER, 2026-09-09: *"We also need a lockdown mode where only local
//! (tui, desktop, repl) frontends are allowed to do anything. Can be
//! enabled from a connector by an admin but disabled only from a local
//! frontend."*
//!
//! WHY THE ASYMMETRY IS THE FEATURE
//!
//! Every other switch in this codebase is symmetric: whoever may turn
//! it on may turn it off. This one must not be, because the situation
//! it exists for is "something is wrong in that channel and I want it
//! to stop NOW" — and the fastest place to say so is the channel
//! itself, from a phone, over IRC. If the same channel could lift it,
//! an attacker who got as far as an admin mask would simply lift it,
//! and the brake would be worth exactly as much as the credential that
//! engaged it.
//!
//! Being at the desktop, the TUI or the REPL is a different KIND of
//! evidence: it is not a role in a config file, it is physical or
//! shell access to the machine veles runs on. USER's choice
//! (2026-09-09) was for any local surface, regardless of how the
//! desktop's listener is bound — widening that bind already takes a
//! token and is the operator's own decision.
//!
//! WHAT IT STOPS
//!
//! Inbound from a connector: no turns, no commands, no tool calls, no
//! ask answers. IRC, Telegram and A2A alike, whatever the sender's
//! role — an owner over IRC is still a connector. The bot stays
//! connected and stays in its channels, because dropping off the
//! network is a different decision (and a noisy one).
//!
//! What it does NOT stop: the local frontends, and cron. Scheduled
//! jobs keep running and keep delivering, including to a connector —
//! they are the owner's own instructions, authored before the
//! lockdown, and stopping them belongs to `/cron pause`. `status()`
//! says so out loud rather than leaving an operator to assume the
//! brake is wider than it is.
//!
//! WHERE THE STATE LIVES
//!
//! Two sources, ORed: `[acl] lockdown = true` in config.toml (which is
//! hot-reloaded, and which a runtime release cannot override — the file
//! is the operator's word), and a record in the store's meta table
//! written by `/lockdown on`. The store is what makes it survive a
//! gateway restart: a brake that a crash-loop releases is not a brake.

use crate::lockutil::LockOk;
use crate::store::Store;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

/// The meta row `/lockdown on` writes.
pub const META_KEY: &str = "lockdown";

/// An engaged lockdown, and who engaged it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lockdown {
    /// The subject that engaged it, or `config.toml`.
    pub by: String,
    /// Unix seconds. 0 for the config-declared one, which has no event.
    pub at: u64,
    /// Free text from the caller ("lifting after the incident").
    pub reason: String,
    /// Declared in config rather than engaged at runtime, which is what
    /// makes it un-releasable from any frontend.
    pub from_config: bool,
}

/// `[acl] lockdown = true` — read live, like every other ACL key.
pub fn from_config() -> bool {
    crate::acl::live_config().lockdown
}

/// The engaged lockdown, if any. Config wins: its record is returned
/// even when a runtime one also exists, because it is the one a
/// frontend cannot lift and an operator needs to be told that.
pub fn current(store: Option<&Arc<Mutex<Store>>>) -> Option<Lockdown> {
    if from_config() {
        return Some(Lockdown {
            by: "config.toml".to_string(),
            at: 0,
            reason: "[acl] lockdown = true".to_string(),
            from_config: true,
        });
    }
    let raw = store?.lock_ok().meta_get(META_KEY)?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(Lockdown {
        by: v["by"].as_str().unwrap_or("unknown").to_string(),
        at: v["at"].as_u64().unwrap_or(0),
        reason: v["reason"].as_str().unwrap_or_default().to_string(),
        from_config: false,
    })
}

/// The question every connector entry point asks.
pub fn engaged(store: Option<&Arc<Mutex<Store>>>) -> bool {
    current(store).is_some()
}

/// Engage it. Idempotent: re-engaging replaces the record, so the
/// reason and the caller stay current.
pub fn engage(store: &Arc<Mutex<Store>>, by: &str, reason: &str) -> anyhow::Result<Lockdown> {
    let rec = Lockdown {
        by: by.to_string(),
        at: now(),
        reason: reason.trim().to_string(),
        from_config: false,
    };
    let raw = serde_json::json!({"by": rec.by, "at": rec.at, "reason": rec.reason});
    store.lock_ok().meta_set(META_KEY, &raw.to_string())?;
    reset_notices();
    Ok(rec)
}

/// Release the RUNTIME record. Returns whether one was there.
///
/// A config-declared lockdown outlives this on purpose: the caller is
/// told to edit the file, because a frontend that could override
/// config.toml would make the file's word meaningless.
pub fn release(store: &Arc<Mutex<Store>>) -> anyhow::Result<bool> {
    let had = store.lock_ok().meta_get(META_KEY).is_some();
    store.lock_ok().meta_delete(META_KEY)?;
    reset_notices();
    Ok(had)
}

/// One line for `/lockdown`, on any surface.
pub fn status(store: Option<&Arc<Mutex<Store>>>) -> String {
    match current(store) {
        None => "lockdown: off — connectors are served normally.".to_string(),
        Some(l) if l.from_config => format!(
            "lockdown: ON, declared in config.toml ({}). No frontend can lift this one — \
             remove `lockdown = true` from `[acl]`. Connectors are refused; the local \
             frontends and cron are unaffected.",
            l.reason
        ),
        Some(l) => {
            let reason = if l.reason.is_empty() {
                String::new()
            } else {
                format!(" — {}", l.reason)
            };
            format!(
                "lockdown: ON since {} by {}{reason}. Connectors are refused; the local \
                 frontends (desktop/TUI/REPL) and cron are unaffected. Lift it with \
                 `/lockdown off` from a LOCAL frontend.",
                stamp(l.at),
                l.by
            )
        }
    }
}

/// What a refused connector sender is told, once.
pub fn refusal(l: &Lockdown) -> String {
    let who = if l.from_config {
        "config.toml".to_string()
    } else {
        l.by.clone()
    };
    format!(
        "[lockdown] veles is locked down by {who} — connectors are not served. \
         It can only be lifted from a local frontend (desktop/TUI/REPL). \
         This is the only notice; further messages are ignored until it is lifted."
    )
}

// ── one notice per sender ────────────────────────────────────────────
//
// A refusal that answers every line turns a locked-down bot into a
// flood engine: a channel with an active conversation would get a
// "veles is locked down" NOTICE per message, which is worse than
// silence and gets the bot killed. So each subject is told once, and
// the set is cleared when the lockdown changes — a second lockdown is
// a new event and deserves a new notice.

fn notified() -> &'static Mutex<HashSet<String>> {
    static N: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    N.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Whether THIS sender should be told. True exactly once per subject
/// per lockdown.
pub fn notice_once(subject: &str) -> bool {
    notified().lock_ok().insert(subject.to_string())
}

fn reset_notices() {
    notified().lock_ok().clear();
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn stamp(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| format!("{secs}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        Arc<Mutex<Store>>,
    ) {
        let (home, env) = crate::testenv::scratch_home();
        let store = Arc::new(Mutex::new(
            Store::open(&home.path().join("state.db")).expect("store"),
        ));
        (home, env, store)
    }

    #[test]
    fn engage_survives_and_release_clears() {
        let (_home, _env, store) = scratch();
        assert!(!engaged(Some(&store)));
        assert!(status(Some(&store)).contains("off"));

        let rec = engage(&store, "irc:localtest/me!me@h", "hostile channel").unwrap();
        assert_eq!(rec.by, "irc:localtest/me!me@h");
        let now = current(Some(&store)).expect("engaged");
        assert_eq!(now.by, rec.by);
        assert_eq!(now.reason, "hostile channel");
        assert!(!now.from_config);
        let s = status(Some(&store));
        assert!(s.contains("ON"), "{s}");
        assert!(s.contains("hostile channel"), "{s}");
        assert!(s.contains("LOCAL"), "the way out is named: {s}");

        // A second engage replaces the record rather than stacking.
        engage(&store, "irc:someone-else", "").unwrap();
        assert_eq!(current(Some(&store)).unwrap().by, "irc:someone-else");

        assert!(release(&store).unwrap(), "there was one to release");
        assert!(!engaged(Some(&store)));
        assert!(!release(&store).unwrap(), "releasing twice is not an error");
    }

    /// A config-declared lockdown is engaged even with an empty store,
    /// and `release` cannot lift it — the file is the operator's word.
    #[test]
    fn config_declared_lockdown_outlives_a_release() {
        let (home, _env, store) = scratch();
        std::fs::write(
            home.path().join("config.toml"),
            "[acl]\nlockdown = true\nowner = [\"local:owner\"]\n",
        )
        .expect("write config");
        crate::config::invalidate_live();
        crate::acl::invalidate_live_config();
        assert!(engaged(Some(&store)));
        let l = current(Some(&store)).expect("engaged");
        assert!(l.from_config);
        assert!(
            status(Some(&store)).contains("config.toml"),
            "an operator must be told which one they are looking at"
        );
        release(&store).unwrap();
        assert!(
            engaged(Some(&store)),
            "a frontend must not be able to override config.toml"
        );
        crate::config::invalidate_live();
        crate::acl::invalidate_live_config();
    }

    /// The refusal reaches each sender once. Anything else makes a
    /// locked-down bot the loudest thing in the channel.
    #[test]
    fn each_sender_is_told_exactly_once() {
        let (_home, _env, store) = scratch();
        engage(&store, "irc:admin", "").unwrap();
        assert!(notice_once("irc:a"));
        assert!(!notice_once("irc:a"));
        assert!(notice_once("irc:b"));
        // A NEW lockdown is a new event: everyone hears about it again.
        engage(&store, "irc:admin", "again").unwrap();
        assert!(notice_once("irc:a"));
        release(&store).unwrap();
        assert!(notice_once("irc:a"));
    }

    /// Nobody reading the refusal should have to guess whether it is
    /// permanent or where to go.
    #[test]
    fn the_refusal_names_the_way_out() {
        let l = Lockdown {
            by: "irc:boss".into(),
            at: 1,
            reason: String::new(),
            from_config: false,
        };
        let r = refusal(&l);
        assert!(r.contains("irc:boss"), "{r}");
        assert!(r.contains("local frontend"), "{r}");
        assert!(r.contains("only notice"), "silence after this: {r}");
    }
}
