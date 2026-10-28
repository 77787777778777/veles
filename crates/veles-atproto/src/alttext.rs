//! Automatic alt text (USER 2026-10-10: "Automatic alt-text for me if I
//! post an image without any alt text, opt-in for mutuals").
//!
//! The answer is a REPLY under the post (the USER's choice: the
//! operator's account lives on bsky.social, so the bot holds no credential
//! that could edit the post itself). What happens:
//!
//! 1. The Jetstream lane carries every post on the network; a post by the
//!    operator (or a mutual who opted in) whose images include one with
//!    no alt is a candidate ([`AltText::candidate`]).
//! 2. After a grace period the post is read again: deleted, or alt added
//!    meanwhile, and nothing is posted.
//! 3. Each image without alt is described by the gateway's vision model
//!    (`vision.describe`), in the post's language, with any text in the
//!    image transcribed.
//! 4. One reply (threaded if long) goes under the post, and the post is
//!    recorded so neither lane answers it twice.
//!
//! Mutuals opt in by mentioning the bot with `/alt on` (`/alt off`,
//! `/alt status`); the relation is checked at opt-in and again before
//! each reply, so an unfollow ends it. Hourly caps bound the spend.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::filter::MatchedEvent;

/// `[platforms.atproto.alt_text]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltSettings {
    pub enabled: bool,
    /// The operator's own posts (mutuals opt in themselves).
    pub operator: bool,
    pub grace_secs: u64,
    pub per_hour: usize,
    pub per_author_per_hour: usize,
}

impl Default for AltSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            operator: true,
            grace_secs: 120,
            per_hour: 30,
            per_author_per_hour: 10,
        }
    }
}

impl AltSettings {
    pub fn from_section(v: &Value) -> Self {
        let d = Self::default();
        let b = |k: &str, def: bool| v.get(k).and_then(Value::as_bool).unwrap_or(def);
        let n = |k: &str, def: u64| v.get(k).and_then(Value::as_u64).unwrap_or(def);
        Self {
            enabled: b("enabled", d.enabled),
            operator: b("operator", d.operator),
            grace_secs: n("grace_secs", d.grace_secs),
            per_hour: n("per_hour", d.per_hour as u64) as usize,
            per_author_per_hour: n("per_author_per_hour", d.per_author_per_hour as u64) as usize,
        }
    }
}

/// What is kept on disk: who opted in (DID → since, unix seconds) and
/// which posts were answered (at-uri → when), so a restart neither drops
/// an opt-in nor answers a post twice.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    optin: BTreeMap<String, u64>,
    #[serde(default)]
    done: BTreeMap<String, u64>,
}

/// The answered-posts record is trimmed to this many, oldest first.
const DONE_KEEP: usize = 5000;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The feature's state, shared by the lane, the command and the worker.
pub struct AltText {
    path: PathBuf,
    ledger: Mutex<Ledger>,
    pub settings: AltSettings,
    operator: String,
    /// `(when, author)` of recent replies, for the hourly caps.
    recent: Mutex<Vec<(u64, String)>>,
    /// Posts a worker is already waiting on: the hint lane can replay an
    /// event, and two workers would both pass `is_done` before either
    /// records the post.
    inflight: Mutex<std::collections::HashSet<String>>,
}

impl AltText {
    pub fn load(state_dir: &Path, settings: AltSettings, operator_did: &str) -> Arc<Self> {
        let path = state_dir.join("alt-text.json");
        let ledger = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Arc::new(Self {
            path,
            ledger: Mutex::new(ledger),
            settings,
            operator: operator_did.to_string(),
            recent: Mutex::new(Vec::new()),
            inflight: Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Claim a post for one worker. False when one already has it.
    pub fn begin(&self, uri: &str) -> bool {
        self.inflight
            .lock()
            .map(|mut s| s.insert(uri.to_string()))
            .unwrap_or(false)
    }

    /// Give up a claim [`begin`](Self::begin) took, without recording the
    /// post as handled. The retry half of a transient failure: a worker
    /// whose vision call timed out releases the post so the next
    /// delivery — a Jetstream replay after a reconnect, or a restart's
    /// cursor resume — can try it again. A post that was answered or
    /// deliberately dropped is never released; it is `mark_done`.
    pub fn release(&self, uri: &str) {
        if let Ok(mut s) = self.inflight.lock() {
            s.remove(uri);
        }
    }

    /// The operator's DID (the one author whose posts need no opt-in).
    pub fn operator(&self) -> &str {
        &self.operator
    }

    fn save(&self, l: &Ledger) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(s) = serde_json::to_string_pretty(l) {
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }

    /// Since when `did` gets alt text, or `None` when it does not.
    pub fn since(&self, did: &str) -> Option<u64> {
        if did == self.operator && self.settings.operator {
            return Some(0);
        }
        self.ledger.lock().ok()?.optin.get(did).copied()
    }

    /// Turn it on or off for a mutual. Returns whether anything changed.
    pub fn set(&self, did: &str, on: bool) -> bool {
        let Ok(mut l) = self.ledger.lock() else {
            return false;
        };
        let changed = if on {
            if l.optin.contains_key(did) {
                false
            } else {
                l.optin.insert(did.to_string(), now_secs());
                true
            }
        } else {
            l.optin.remove(did).is_some()
        };
        if changed {
            let snapshot = l.clone();
            drop(l);
            self.save(&snapshot);
        }
        changed
    }

    pub fn is_done(&self, uri: &str) -> bool {
        self.ledger
            .lock()
            .map(|l| l.done.contains_key(uri))
            .unwrap_or(true)
    }

    /// Record a post as handled (answered, or deliberately not).
    pub fn mark_done(&self, uri: &str) {
        let Ok(mut l) = self.ledger.lock() else {
            return;
        };
        l.done.insert(uri.to_string(), now_secs());
        if l.done.len() > DONE_KEEP {
            let mut by_age: Vec<(u64, String)> =
                l.done.iter().map(|(k, v)| (*v, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(l.done.len() - DONE_KEEP) {
                l.done.remove(&k);
            }
        }
        let snapshot = l.clone();
        drop(l);
        self.save(&snapshot);
    }

    /// Is this event a post that should get alt text? Cheap: no network.
    pub fn candidate(&self, ev: &MatchedEvent) -> bool {
        if !self.settings.enabled {
            return false;
        }
        let Some(since) = self.since(&ev.author_did) else {
            return false;
        };
        if ev.record.get("$type").and_then(Value::as_str) != Some("app.bsky.feed.post") {
            return false;
        }
        // Only posts made after the opt-in (the event time; the record's
        // own createdAt is the author's claim).
        let at = ev
            .at_us
            .map(|u| (u / 1_000_000) as u64)
            .unwrap_or_else(now_secs);
        if at < since {
            return false;
        }
        !missing_alt(&ev.record).is_empty() && !self.is_done(&ev.post_uri)
    }

    /// Spend one reply against the hourly caps, or say which one is full.
    pub fn claim(&self, did: &str) -> Result<(), String> {
        let mut r = self.recent.lock().map_err(|_| "poisoned".to_string())?;
        let cutoff = now_secs().saturating_sub(3600);
        r.retain(|(t, _)| *t > cutoff);
        if r.len() >= self.settings.per_hour {
            return Err(format!("{} alt-text replies in the last hour", r.len()));
        }
        let mine = r.iter().filter(|(_, d)| d == did).count();
        if mine >= self.settings.per_author_per_hour {
            return Err(format!(
                "{mine} alt-text replies for {did} in the last hour"
            ));
        }
        r.push((now_secs(), did.to_string()));
        Ok(())
    }
}

/// The images of a post with no alt: `(1-based position, blob CID)`.
pub fn missing_alt(record: &Value) -> Vec<(usize, String)> {
    let Some(embed) = record.get("embed") else {
        return Vec::new();
    };
    crate::rich::embed_images(embed)
        .iter()
        .enumerate()
        .filter(|(_, im)| {
            im.get("alt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .is_empty()
        })
        .map(|(i, im)| (i + 1, crate::rich::image_cid(im)))
        .filter(|(_, cid)| !cid.is_empty())
        .collect()
}

/// How many images a post carries.
pub fn image_count(record: &Value) -> usize {
    record
        .get("embed")
        .map(|e| crate::rich::embed_images(e).len())
        .unwrap_or(0)
}

/// `/alt on|off|status` (also `/alttext`), the bot's mention stripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    On,
    Off,
    Status,
}

pub fn command(text: &str) -> Option<Command> {
    let t = text.trim().to_ascii_lowercase();
    let mut words = t.split_whitespace();
    let head = words.next()?;
    if head != "/alt" && head != "/alttext" {
        return None;
    }
    Some(match words.next() {
        Some("on") | Some("yes") | Some("enable") => Command::On,
        Some("off") | Some("no") | Some("stop") | Some("disable") => Command::Off,
        _ => Command::Status,
    })
}

/// The vision model's instructions for one image.
pub fn prompt(post_text: &str, lang: &str) -> String {
    let lang = if lang.trim().is_empty() {
        "en"
    } else {
        lang.trim()
    };
    let mut p = format!(
        "Write alt text for this image, for someone who cannot see it, as it appears in a \
         Bluesky post. Describe what matters in one to three sentences. If the image \
         contains text, transcribe it exactly. Write in the language with the BCP-47 code \
         `{lang}`. Answer with the alt text only, no preamble, at most 900 characters."
    );
    let post: String = post_text.trim().chars().take(300).collect();
    if !post.is_empty() {
        p.push_str(&format!(
            "\n\nThe post's own text, for context (do not repeat it): «{post}»"
        ));
    }
    p
}

/// A model's answer as alt text: no "Alt text:" lead, no wrapping
/// quotes, within Bluesky's 1000-character alt limit.
pub fn clean(s: &str) -> String {
    let mut t = s.trim();
    for lead in ["alt text:", "alt-text:", "alt:"] {
        if t.len() >= lead.len() && t[..lead.len()].eq_ignore_ascii_case(lead) {
            t = t[lead.len()..].trim_start();
        }
    }
    let t = t.trim_matches(|c| c == '"' || c == '“' || c == '”').trim();
    t.chars().take(1000).collect()
}

/// The reply: one image → "Alt text: …"; several → one paragraph per
/// image that had none, numbered by its place in the post.
pub fn reply_text(descs: &[(usize, String)], total: usize) -> String {
    if total <= 1 && descs.len() == 1 {
        return format!("Alt text: {}", descs[0].1);
    }
    descs
        .iter()
        .map(|(i, d)| format!("Alt text for image {i}: {d}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One image described by the gateway's vision model
/// (`vision.describe`), in `lang`.
pub async fn describe(
    host: &veles_plugin_sdk::Host,
    url: &str,
    post_text: &str,
    lang: &str,
) -> Option<String> {
    let v = host
        .call_service(
            veles_plugin_sdk::proto::method::VISION_DESCRIBE,
            serde_json::json!({"url": url, "prompt": prompt(post_text, lang)}),
            None,
        )
        .await
        .map_err(|e| tracing::warn!("atproto: vision.describe failed: {e}"))
        .ok()?;
    let text = v.get("text")?.as_str()?.trim().to_string();
    let text = clean(&text);
    (!text.is_empty()).then_some(text)
}

/// Handle one candidate: wait, re-read, describe, reply, record.
pub async fn handle(
    alt: Arc<AltText>,
    client: crate::pds::PdsClient,
    host: veles_plugin_sdk::Host,
    resolver: Arc<crate::identity::IdentityResolver>,
    ev: MatchedEvent,
) {
    tokio::time::sleep(std::time::Duration::from_secs(alt.settings.grace_secs)).await;
    let uri = ev.post_uri.clone();
    if alt.is_done(&uri) {
        return;
    }
    let Ok((repo, collection, rkey)) = crate::post::parse_at_uri(&uri) else {
        return;
    };
    // Read it again: deleted, or alt added in the meantime, is the end.
    let now = match client.get_record_appview(&repo, &collection, &rkey).await {
        Ok(v) => v,
        Err(e) => {
            tracing::info!("atproto: alt text for {uri}: the post is gone or unreadable ({e:#})");
            alt.mark_done(&uri);
            return;
        }
    };
    let record = now.get("value").cloned().unwrap_or(Value::Null);
    let cid = now
        .get("cid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let missing = missing_alt(&record);
    if missing.is_empty() || cid.is_empty() {
        tracing::info!(
            "atproto: alt text for {uri}: alt was added (or no image left); nothing to do"
        );
        alt.mark_done(&uri);
        return;
    }
    // A mutual who has since unfollowed is not answered.
    if ev.author_did != alt.operator {
        let r = resolver.relation(&ev.author_did).await;
        if !opted_in_relation(r) {
            tracing::info!(
                "atproto: alt text for {uri}: {} is no longer a mutual ({r:?}); skipped",
                ev.author_did
            );
            alt.mark_done(&uri);
            return;
        }
    }
    if let Err(why) = alt.claim(&ev.author_did) {
        tracing::info!("atproto: alt text for {uri} skipped: {why}");
        alt.mark_done(&uri);
        return;
    }
    let lang = record
        .get("langs")
        .and_then(Value::as_array)
        .and_then(|l| l.first())
        .and_then(Value::as_str)
        .unwrap_or("en")
        .to_string();
    let post_text = record.get("text").and_then(Value::as_str).unwrap_or("");
    let mut descs = Vec::new();
    for (i, blob) in &missing {
        let url = crate::rich::cdn_image_url(&ev.author_did, blob);
        if let Some(d) = describe(&host, &url, post_text, &lang).await {
            descs.push((*i, d));
        }
    }
    if descs.is_empty() {
        // Nothing described: a transient failure (the vision route timed
        // out, the model refused). Release the claim and leave the post
        // UNdone, so the next delivery of it can try again. Recording it
        // here was the bug that made one slow model call permanent: the
        // post went into the ledger as handled and was never retried,
        // though the image still had no alt text.
        tracing::warn!(
            "atproto: alt text for {uri}: the vision model answered nothing; left unhandled for a retry"
        );
        alt.release(&uri);
        return;
    }
    // From here the reply is being written, so the post is handled even if
    // a later `create_post` fails: retrying after a partial thread would
    // double-post the parts that did go out.
    alt.mark_done(&uri);
    let text = reply_text(&descs, image_count(&record));
    // Threaded under the post; the root is the post's own root when it is
    // itself a reply.
    let parent = (uri.clone(), cid);
    let root = record
        .get("reply")
        .and_then(|r| r.get("root"))
        .and_then(|r| {
            Some((
                r.get("uri")?.as_str()?.to_string(),
                r.get("cid")?.as_str()?.to_string(),
            ))
        })
        .unwrap_or_else(|| parent.clone());
    let mut cur = parent;
    for (n, chunk) in crate::post::split_thread(&text).iter().enumerate() {
        let reply = crate::post::build_reply(root.clone(), cur.clone());
        let rec = crate::post::assemble_post(
            chunk,
            std::slice::from_ref(&lang),
            serde_json::Value::Null,
            Some(reply),
            None,
        );
        match client.create_post(rec).await {
            Ok((u, c)) => {
                tracing::info!("atproto: alt text for {uri} posted ({u}, part {})", n + 1);
                cur = (u, c);
            }
            Err(e) => {
                tracing::warn!("atproto: alt text for {uri}: posting failed: {e:#}");
                return;
            }
        }
    }
}

/// Who may still be answered on an opt-in, checked again before each
/// reply: a mutual of the operator, or a co-operator (an account the
/// operator runs, `platforms.atproto.co_operators`).
pub(crate) fn opted_in_relation(r: crate::identity::Relation) -> bool {
    matches!(
        r,
        crate::identity::Relation::Mutual | crate::identity::Relation::CoOperator
    )
}

#[cfg(test)]
mod tests {
    /// The re-check before each reply admits a mutual and a co-operator
    /// and nobody else.
    #[test]
    fn the_reply_recheck_admits_mutuals_and_co_operators() {
        use crate::identity::Relation;
        assert!(super::opted_in_relation(Relation::Mutual));
        assert!(super::opted_in_relation(Relation::CoOperator));
        for r in [Relation::Follower, Relation::Following, Relation::Stranger] {
            assert!(!super::opted_in_relation(r), "{r:?}");
        }
    }

    use super::*;
    use serde_json::json;

    fn post(alts: &[&str]) -> Value {
        let images: Vec<Value> = alts
            .iter()
            .enumerate()
            .map(|(i, a)| json!({"alt": a, "image": {"ref": {"$link": format!("bafy{i}")}}}))
            .collect();
        json!({"$type": "app.bsky.feed.post", "text": "look", "langs": ["de"],
               "embed": {"$type": "app.bsky.embed.images", "images": images}})
    }

    fn ev(did: &str, record: Value, at_secs: u64) -> MatchedEvent {
        MatchedEvent {
            lane: crate::event::Lane::Jetstream,
            key: "at://x#c".into(),
            thread: "at://x".into(),
            author_did: did.into(),
            author_handle: None,
            text: String::new(),
            post_uri: format!("at://{did}/app.bsky.feed.post/1"),
            record,
            cursor: None,
            at_us: Some((at_secs * 1_000_000) as i64),
        }
    }

    #[test]
    fn the_images_without_alt_are_found_in_both_embed_shapes() {
        assert_eq!(
            missing_alt(&post(&["a cat", "", "  "])),
            [(2, "bafy1".into()), (3, "bafy2".into())]
        );
        assert!(missing_alt(&post(&["a cat"])).is_empty());
        let rwm = json!({"embed": {"$type": "app.bsky.embed.recordWithMedia",
            "media": {"$type": "app.bsky.embed.images", "images": [{"alt": "", "image": {"ref": {"$link": "q"}}}]}}});
        assert_eq!(missing_alt(&rwm), [(1, "q".into())]);
        assert!(missing_alt(&json!({"text": "no embed"})).is_empty());
    }

    #[test]
    fn the_operator_and_opted_in_mutuals_are_candidates_and_nobody_else() {
        let dir = tempfile::tempdir().unwrap();
        let alt = AltText::load(dir.path(), AltSettings::default(), "did:plc:op");
        let now = now_secs();
        assert!(
            alt.candidate(&ev("did:plc:op", post(&[""]), now)),
            "the operator"
        );
        assert!(
            !alt.candidate(&ev("did:plc:op", post(&["alt"]), now)),
            "alt present"
        );
        assert!(
            !alt.candidate(&ev("did:plc:m", post(&[""]), now)),
            "a mutual who did not opt in"
        );
        assert!(alt.set("did:plc:m", true));
        assert!(!alt.set("did:plc:m", true), "already on");
        assert!(alt.candidate(&ev("did:plc:m", post(&[""]), now + 5)));
        assert!(
            !alt.candidate(&ev("did:plc:m", post(&[""]), now.saturating_sub(3600))),
            "a post from before the opt-in"
        );
        let e = ev("did:plc:m", post(&[""]), now + 5);
        alt.mark_done(&e.post_uri);
        assert!(!alt.candidate(&e), "answered once");
        // It survives a restart.
        let again = AltText::load(dir.path(), AltSettings::default(), "did:plc:op");
        assert!(again.since("did:plc:m").is_some());
        assert!(again.is_done(&e.post_uri));
        assert!(again.set("did:plc:m", false));
        assert!(again.since("did:plc:m").is_none());
        let off = AltText::load(
            dir.path(),
            AltSettings {
                operator: false,
                ..Default::default()
            },
            "did:plc:op",
        );
        assert!(
            !off.candidate(&ev("did:plc:op", post(&[""]), now)),
            "operator switched off"
        );
    }

    #[test]
    fn one_worker_per_post() {
        let dir = tempfile::tempdir().unwrap();
        let alt = AltText::load(dir.path(), AltSettings::default(), "did:plc:op");
        assert!(alt.begin("at://p"));
        assert!(
            !alt.begin("at://p"),
            "a replayed event finds the post claimed"
        );
        assert!(alt.begin("at://q"));
    }

    /// A transient failure (the vision route timed out) releases the
    /// claim without recording the post, so the next delivery can try
    /// again. This is the retry half of the fix: `begin` alone never
    /// gave a claim back, so one slow model call left the post both
    /// claimed forever and — because the old code marked it done —
    /// permanently unanswered.
    #[test]
    fn a_released_claim_is_open_again_but_a_done_post_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let alt = AltText::load(dir.path(), AltSettings::default(), "did:plc:op");
        let e = ev("did:plc:op", post(&[""]), now_secs());

        assert!(alt.begin(&e.post_uri), "first claim");
        alt.release(&e.post_uri);
        assert!(!alt.is_done(&e.post_uri), "released is not done");
        assert!(
            alt.begin(&e.post_uri),
            "a released post can be claimed again"
        );
        assert!(alt.candidate(&e), "and is still a candidate");

        alt.mark_done(&e.post_uri);
        assert!(!alt.begin(&e.post_uri), "a done post stays claimed out");
        assert!(!alt.candidate(&e), "and is no longer a candidate");
        // It survives a restart, so a done post is never re-answered.
        let again = AltText::load(dir.path(), AltSettings::default(), "did:plc:op");
        assert!(again.is_done(&e.post_uri));
    }

    #[test]
    fn the_caps_bound_the_spend() {
        let dir = tempfile::tempdir().unwrap();
        let alt = AltText::load(
            dir.path(),
            AltSettings {
                per_hour: 3,
                per_author_per_hour: 2,
                ..Default::default()
            },
            "did:plc:op",
        );
        assert!(alt.claim("a").is_ok());
        assert!(alt.claim("a").is_ok());
        assert!(alt.claim("a").unwrap_err().contains("for a"));
        assert!(alt.claim("b").is_ok());
        assert!(alt.claim("c").unwrap_err().contains("last hour"));
    }

    #[test]
    fn the_command_the_reply_and_the_cleanup() {
        assert_eq!(command("/alt on"), Some(Command::On));
        assert_eq!(command("/ALTTEXT off"), Some(Command::Off));
        assert_eq!(command("/alt"), Some(Command::Status));
        assert_eq!(command("/alternative"), None);
        assert_eq!(command("hello /alt on"), None);
        assert_eq!(reply_text(&[(1, "a cat".into())], 1), "Alt text: a cat");
        assert_eq!(
            reply_text(&[(2, "a dog".into()), (3, "a sign".into())], 3),
            "Alt text for image 2: a dog\n\nAlt text for image 3: a sign"
        );
        assert_eq!(clean("Alt text: \"a cat on a mat\""), "a cat on a mat");
        assert_eq!(clean(&"x".repeat(2000)).chars().count(), 1000);
        let p = prompt("my lunch", "de");
        assert!(p.contains("`de`") && p.contains("my lunch") && p.contains("transcribe"));
    }
}
