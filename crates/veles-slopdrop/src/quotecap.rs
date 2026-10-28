//! How often a reply asked by anyone but the owner may QUOTE a post
//! (PLAN-slopdrop-art.md, P0).
//!
//! A quote notifies the quoted post's author, and the account quoting is
//! the bot's. `bsky_quote [bsky_get_parent] {…}` makes quoting a post
//! with a drawing of its words one line of Tcl, so the bot must not
//! become a way to pile on one person: anyone but the owner gets at most
//! [`PER_AUTHOR`] quotes of one author per day, and one per post.
//!
//! The counts live in `<state>/quotes.json`, the quotes that went out in
//! the last [`WINDOW_SECS`] (pruned on every write). The withheld ledger
//! cannot count them: it folds the same output into one entry.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Quotes of one author's posts, per day, from anyone but the owner.
pub const PER_AUTHOR: usize = 3;
/// The window the counts cover.
pub const WINDOW_SECS: u64 = 24 * 3600;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Quoted {
    at: u64,
    /// The quoted post's at-uri (fragment dropped).
    uri: String,
    /// Its author's DID (the at-uri's repo).
    author: String,
    /// Who asked (display only).
    requester: String,
}

fn path() -> PathBuf {
    crate::data::dir().join("quotes.json")
}

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load(at: u64) -> Vec<Quoted> {
    let all: Vec<Quoted> = std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    all.into_iter()
        .filter(|q| at.saturating_sub(q.at) < WINDOW_SECS)
        .collect()
}

/// Why this quote may not go out now, or `None` when it may. Read-only.
pub fn refusal(uri: &str) -> Option<String> {
    refusal_at(uri, now())
}

fn refusal_at(uri: &str, at: u64) -> Option<String> {
    let _g = lock().lock().unwrap_or_else(|p| p.into_inner());
    let author = crate::bsky::uri_repo(uri).unwrap_or("").to_string();
    let recent = load(at);
    if recent.iter().any(|q| q.uri == uri) {
        return Some("(that post was quoted already today; one quote per post)".into());
    }
    if recent.iter().filter(|q| q.author == author).count() >= PER_AUTHOR {
        return Some(format!(
            "(that author was quoted {PER_AUTHOR} times today; no more quotes of them until tomorrow)"
        ));
    }
    None
}

/// Count a quote that is going out.
pub fn record(uri: &str, requester: &str) -> Result<(), String> {
    record_at(uri, requester, now())
}

fn record_at(uri: &str, requester: &str, at: u64) -> Result<(), String> {
    let _g = lock().lock().unwrap_or_else(|p| p.into_inner());
    let mut recent = load(at);
    recent.push(Quoted {
        at,
        uri: uri.to_string(),
        author: crate::bsky::uri_repo(uri).unwrap_or("").to_string(),
        requester: requester.to_string(),
    });
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let body = serde_json::to_string_pretty(&recent).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(author: &str, n: u32) -> String {
        format!("at://{author}/app.bsky.feed.post/{n}")
    }

    /// One quote per post, [`PER_AUTHOR`] per author, and a day later the
    /// counts are gone.
    #[test]
    fn quotes_of_one_author_are_capped_per_day_and_per_post() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let t = 1_800_000_000;
        assert_eq!(refusal_at(&post("did:plc:a", 1), t), None);
        record_at(&post("did:plc:a", 1), "x", t).unwrap();
        assert!(
            refusal_at(&post("did:plc:a", 1), t + 1).is_some(),
            "the same post twice"
        );
        for n in 2..=PER_AUTHOR as u32 {
            assert_eq!(refusal_at(&post("did:plc:a", n), t + 2), None, "{n}");
            record_at(&post("did:plc:a", n), "x", t + 2).unwrap();
        }
        let full = refusal_at(&post("did:plc:a", 99), t + 3).expect("capped");
        assert!(full.contains("3 times"), "{full}");
        assert_eq!(
            refusal_at(&post("did:plc:b", 1), t + 3),
            None,
            "another author is not affected"
        );
        assert_eq!(
            refusal_at(&post("did:plc:a", 99), t + 2 + WINDOW_SECS),
            None,
            "a day later the counts are gone"
        );
    }
}
