//! The atproto event model + the mention-gate — the security-critical
//! parse layer between an untrusted network event and an agent turn.
//!
//! A Jetstream commit or a firehose event is JSON we did not author and
//! cannot trust (Jetstream carries no signatures; a firehose commit is
//! signed but we do not verify it here — verification is the consumer's
//! authenticated re-fetch). So this layer does exactly two things:
//! decode enough structure to ROUTE the event, and decide whether it is
//! *addressed to the bot*. It never authors a turn by itself.

use serde::Deserialize;

/// The subset of a Jetstream v2 / firehose commit we route on.
/// Fields we do not read are simply absent — serde ignores them.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitEvent {
    /// The repo (author) DID. On Jetstream this is top-level `did`; on a
    /// firehose `#commit` it is `repo`. The parser normalizes into here.
    pub did: String,
    /// The collection NSID (`app.bsky.feed.post`, …).
    #[serde(default)]
    pub collection: String,
    /// The record key.
    #[serde(default)]
    pub rkey: String,
    /// create | update | delete.
    #[serde(default)]
    pub operation: String,
    /// The record body (a post's text/facets/reply), when present.
    #[serde(default)]
    pub record: Option<serde_json::Value>,
    /// The commit CID, when the lane carries one (stability for dedup).
    #[serde(default)]
    pub cid: Option<String>,
    /// The lane's sequence number, when it carries one (cursor resume).
    #[serde(default)]
    pub seq: Option<i64>,
}

/// A post's reply block (`app.bsky.feed.post.reply`).
#[derive(Debug, Clone, Deserialize)]
pub struct ReplyRefs {
    #[serde(default)]
    pub root: Option<StrongRef>,
    #[serde(default)]
    pub parent: Option<StrongRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrongRef {
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub cid: String,
}

/// Which lane an event arrived on. The lane decides how much we trust
/// it: a Firehose commit is signed by the hosting PDS (authoritative for
/// repos WE host), Jetstream is a signatureless hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Public Jetstream v2 — untrusted hint, triggers re-fetch/reconcile.
    Jetstream,
    /// Local vlpds `subscribeRepos` — signed, repos WE host.
    Firehose,
    /// The notification poll (`listNotifications`) — authenticated, on
    /// our own PDS. The durable backstop.
    Notification,
}

/// The mention-gate verdict for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Addressed to the bot: a mention facet naming the bot DID, or a
    /// reply whose parent/root is one of the bot's posts.
    Addressed,
    /// Not addressed: watched, never answered.
    Ignored,
}

/// Is this post event addressed to the bot? Pure and total, so the test
/// can pin every arm.
///
/// Two ways in, both DID-anchored (a handle is re-pointable, a DID is
/// not):
///
/// 1. A `app.bsky.richtext.facet#mention` feature whose `did` is the
///    bot's. This is the explicit @-mention.
/// 2. A reply whose `parent.uri` (or `root.uri`) names the bot's repo —
///    someone answered the bot's post. The at-uri is
///    `at://<bot_did>/app.bsky.feed.post/…`.
///
/// Everything else — a bare text mention with no facet, a mention of a
/// *handle* that happens to look like ours, an unrelated post — is
/// Ignored. A facet that fails to parse is Ignored, never a crash and
/// never a turn.
pub fn gate_post(bot_did: &str, record: &serde_json::Value) -> Gate {
    if bot_did.is_empty() {
        return Gate::Ignored;
    }
    // (2) reply-to-bot: cheap to check first.
    if let Some(reply) = record.get("reply") {
        let names_bot = |key: &str| {
            reply
                .get(key)
                .and_then(|r| r.get("uri"))
                .and_then(|u| u.as_str())
                .is_some_and(|uri| uri_prefix_is_bot(uri, bot_did))
        };
        if names_bot("parent") || names_bot("root") {
            return Gate::Addressed;
        }
    }
    // (1) a mention facet naming the bot DID.
    if let Some(facets) = record.get("facets").and_then(|f| f.as_array()) {
        for facet in facets {
            let Some(features) = facet.get("features").and_then(|f| f.as_array()) else {
                continue;
            };
            for feat in features {
                let is_mention = feat
                    .get("$type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == "app.bsky.richtext.facet#mention");
                if !is_mention {
                    continue;
                }
                let did = feat.get("did").and_then(|d| d.as_str()).unwrap_or("");
                if did == bot_did {
                    return Gate::Addressed;
                }
            }
        }
    }
    Gate::Ignored
}

/// Is this post a turn for the BOT? Stricter than [`gate_post`], which
/// stays the broad "concerns this DID" match courier's notifications
/// want (a reply anywhere under your post notifies you):
///
/// - **Never the bot's own post.** A long reply is a self-replying
///   thread, so the bot's continuations are replies to the bot; through
///   `gate_post` they reached the door, and only the door, because the
///   bot followed nobody, kept the bot from answering itself on a public
///   feed. Once it was a mutual of the operator, or with
///   `strangers_policy = "guest"`, it would have looped.
/// - **Explicitly addressed.** A mention facet naming the bot, or a reply
///   whose PARENT is the bot's post (someone answered it). A reply that
///   merely sits under a thread the bot ROOTED is two other people
///   talking (USER 2026-10-04: "responds when explicitly addressed").
pub fn gate_bot_post(bot_did: &str, author_did: &str, record: &serde_json::Value) -> Gate {
    if bot_did.is_empty() || author_did == bot_did {
        return Gate::Ignored;
    }
    let parent_is_bot = record
        .get("reply")
        .and_then(|r| r.get("parent"))
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
        .is_some_and(|uri| uri_prefix_is_bot(uri, bot_did));
    if parent_is_bot || mentions(bot_did, record) {
        Gate::Addressed
    } else {
        Gate::Ignored
    }
}

/// Does the record carry a mention facet naming `did`?
fn mentions(did: &str, record: &serde_json::Value) -> bool {
    let Some(facets) = record.get("facets").and_then(|f| f.as_array()) else {
        return false;
    };
    facets
        .iter()
        .filter_map(|f| f.get("features").and_then(|f| f.as_array()))
        .flatten()
        .any(|feat| {
            feat.get("$type").and_then(|t| t.as_str()) == Some("app.bsky.richtext.facet#mention")
                && feat.get("did").and_then(|d| d.as_str()) == Some(did)
        })
}

/// Does this at-uri name a record in the bot's own repo?
/// `at://did:plc:XYZ/app.bsky.feed.post/abc` → repo `did:plc:XYZ`.
fn uri_prefix_is_bot(uri: &str, bot_did: &str) -> bool {
    uri.strip_prefix("at://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|repo| repo == bot_did)
}

/// The stable dedup/routing id for an event: `at://did/collection/rkey`,
/// plus the cid when the lane carried one. Consumers claim this in their
/// own durable ledger before dispatch (exactly-once).
pub fn event_key(ev: &CommitEvent) -> String {
    match &ev.cid {
        Some(cid) if !cid.is_empty() => {
            format!("at://{}/{}/{}#{cid}", ev.did, ev.collection, ev.rkey)
        }
        _ => format!("at://{}/{}/{}", ev.did, ev.collection, ev.rkey),
    }
}

/// The thread a reply belongs in. A reply's chat_id is the thread ROOT
/// (so a whole conversation is one session); a top-level mention's is
/// its own uri (it starts the thread). Falls back to the event's own
/// uri when the record has no reply block.
pub fn thread_key(ev: &CommitEvent) -> String {
    if let Some(rec) = &ev.record {
        if let Ok(reply) = serde_json::from_value::<ReplyRefs>(
            rec.get("reply").cloned().unwrap_or(serde_json::Value::Null),
        ) {
            if let Some(root) = reply.root.filter(|r| !r.uri.is_empty()) {
                return root.uri;
            }
            if let Some(parent) = reply.parent.filter(|r| !r.uri.is_empty()) {
                return parent.uri;
            }
        }
    }
    format!("at://{}/{}/{}", ev.did, ev.collection, ev.rkey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BOT: &str = "did:plc:bot123";

    fn mention_facet(did: &str) -> serde_json::Value {
        json!([{ "index": {"byteStart":0,"byteEnd":5},
                 "features":[{"$type":"app.bsky.richtext.facet#mention","did":did}] }])
    }

    #[test]
    fn a_mention_facet_naming_the_bot_is_addressed() {
        let rec = json!({"text":"hi @bot","facets":mention_facet(BOT)});
        assert_eq!(gate_post(BOT, &rec), Gate::Addressed);
    }

    #[test]
    fn a_mention_of_someone_else_is_ignored() {
        let rec = json!({"text":"hi @other","facets":mention_facet("did:plc:other")});
        assert_eq!(gate_post(BOT, &rec), Gate::Ignored);
    }

    #[test]
    fn bare_text_with_no_facet_is_ignored() {
        // Even if the text literally contains the handle: no facet, no turn.
        let rec = json!({"text":"hi veles.jackma.7778777.online"});
        assert_eq!(gate_post(BOT, &rec), Gate::Ignored);
    }

    #[test]
    fn a_reply_to_a_bot_post_is_addressed() {
        let rec = json!({"text":"reply","reply":{
            "root":{"uri":format!("at://{BOT}/app.bsky.feed.post/1"),"cid":"c"},
            "parent":{"uri":format!("at://{BOT}/app.bsky.feed.post/1"),"cid":"c"}}});
        assert_eq!(gate_post(BOT, &rec), Gate::Addressed);
    }

    #[test]
    fn a_reply_rooted_at_the_bot_but_parenting_someone_else_is_addressed() {
        // Deep thread: root is the bot's, parent is another replier.
        let rec = json!({"text":"reply","reply":{
            "root":{"uri":format!("at://{BOT}/app.bsky.feed.post/1"),"cid":"c"},
            "parent":{"uri":"at://did:plc:other/app.bsky.feed.post/2","cid":"c"}}});
        assert_eq!(gate_post(BOT, &rec), Gate::Addressed);
    }

    #[test]
    fn a_reply_in_an_unrelated_thread_is_ignored() {
        let rec = json!({"text":"reply","reply":{
            "root":{"uri":"at://did:plc:a/app.bsky.feed.post/1","cid":"c"},
            "parent":{"uri":"at://did:plc:b/app.bsky.feed.post/2","cid":"c"}}});
        assert_eq!(gate_post(BOT, &rec), Gate::Ignored);
    }

    #[test]
    fn the_bots_own_posts_are_never_a_turn_for_it() {
        // Its thread continuation (a reply to itself) and even a post
        // that mentions itself: the live log had the bot's own posts at
        // the door four times; only "not a mutual" turned them away.
        let continuation = json!({"text":"…part 2","reply":{
            "root":{"uri":"at://did:plc:op/app.bsky.feed.post/1","cid":"c"},
            "parent":{"uri":format!("at://{BOT}/app.bsky.feed.post/2"),"cid":"c"}}});
        assert_eq!(gate_bot_post(BOT, BOT, &continuation), Gate::Ignored);
        let self_mention = json!({"text":"hi @bot","facets":mention_facet(BOT)});
        assert_eq!(gate_bot_post(BOT, BOT, &self_mention), Gate::Ignored);
        // The same records from someone else ARE addressed.
        assert_eq!(
            gate_bot_post(BOT, "did:plc:op", &continuation),
            Gate::Addressed
        );
        assert_eq!(
            gate_bot_post(BOT, "did:plc:op", &self_mention),
            Gate::Addressed
        );
    }

    #[test]
    fn a_reply_only_rooted_at_the_bot_is_not_a_turn() {
        // Two people talking under a thread the bot started: nobody
        // addressed the bot. (Courier's broad gate still matches it.)
        let rec = json!({"text":"reply","reply":{
            "root":{"uri":format!("at://{BOT}/app.bsky.feed.post/1"),"cid":"c"},
            "parent":{"uri":"at://did:plc:other/app.bsky.feed.post/2","cid":"c"}}});
        assert_eq!(gate_bot_post(BOT, "did:plc:a", &rec), Gate::Ignored);
        assert_eq!(gate_post(BOT, &rec), Gate::Addressed);
    }

    #[test]
    fn an_empty_bot_did_never_addresses() {
        // Fail-closed: an unconfigured identity answers nothing.
        let rec = json!({"text":"hi","facets":mention_facet(BOT)});
        assert_eq!(gate_post("", &rec), Gate::Ignored);
    }

    #[test]
    fn a_malformed_facet_is_ignored_not_a_crash() {
        let rec = json!({"text":"hi","facets":[{"features":[{"$type":"app.bsky.richtext.facet#mention"}]}]});
        assert_eq!(gate_post(BOT, &rec), Gate::Ignored);
    }

    #[test]
    fn event_key_prefers_the_cid_for_stability() {
        let ev = CommitEvent {
            did: "did:plc:a".into(),
            collection: "app.bsky.feed.post".into(),
            rkey: "rk".into(),
            operation: "create".into(),
            record: None,
            cid: Some("bafy".into()),
            seq: None,
        };
        assert_eq!(event_key(&ev), "at://did:plc:a/app.bsky.feed.post/rk#bafy");
        let no_cid = CommitEvent { cid: None, ..ev };
        assert_eq!(event_key(&no_cid), "at://did:plc:a/app.bsky.feed.post/rk");
    }

    #[test]
    fn a_reply_threads_to_its_root_a_top_post_to_itself() {
        let reply_ev = CommitEvent {
            did: "did:plc:a".into(),
            collection: "app.bsky.feed.post".into(),
            rkey: "rk".into(),
            operation: "create".into(),
            record: Some(json!({"reply":{
                "root":{"uri":"at://did:plc:z/app.bsky.feed.post/root","cid":"c"},
                "parent":{"uri":"at://did:plc:y/app.bsky.feed.post/p","cid":"c"}}})),
            cid: None,
            seq: None,
        };
        assert_eq!(
            thread_key(&reply_ev),
            "at://did:plc:z/app.bsky.feed.post/root"
        );
        let top_ev = CommitEvent {
            record: Some(json!({"text":"hi"})),
            ..reply_ev.clone()
        };
        assert_eq!(thread_key(&top_ev), "at://did:plc:a/app.bsky.feed.post/rk");
    }
}
