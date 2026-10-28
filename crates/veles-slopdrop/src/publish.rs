//! The publisher side of the public `/tcl` ledger: repost and takedown.
//!
//! The ledger (`withheld`) is platform-neutral — it only knows the
//! output and the reason. Acting on it is not: an approve reposts a
//! withheld body inline under the original request on the PUBLIC feed, a
//! delete takes any recorded reply's post(s), short link and owner-pointer
//! record down. Both need the PDS session, which lives in the atproto
//! plugin (a process plugin), not here. So this module DECIDES what is
//! reposted or taken down and asks the plugin to move the bytes through
//! its GENERIC repo services (`post.create`, `repo.delete_record`,
//! `hosted.delete` — USER 2026-10-09: the plugin keeps no slopdrop code),
//! found through the service registry (`veles_core::services`), never by
//! plugin name.
//!
//! `Blocked`/`Approved` bookkeeping (the allowlist/bad-list writes) is
//! ours and happens HERE, synchronously, before the plugin is asked to
//! move bytes: a repost that never lands still allowlisted the text is a
//! smaller lie than the reverse, and the plugin reports its own failure.

use veles_types::plugin as proto;

use crate::safety::{self, Verdict};
use crate::withheld::{output_hash, PostKind, WithheldEntry, WithheldStore};

/// The two things the owner can do with a recorded reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithheldAction {
    /// Repost the output inline under the request, allowlist the exact
    /// text, and (one stored proc) mark the proc Approved.
    Approve,
    /// Take the post(s), the short link and the pointer record down; mark
    /// the output hash and the proc Blocked.
    Delete,
}

/// What the plugin did for one action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Acted {
    /// A short human report (what was reposted / what came down).
    pub report: String,
    /// The at-uris of the posts it made (an approve's inline repost), so
    /// the entry can name them and a later delete can take them down.
    pub posted: Vec<String>,
}

/// The collection a Bluesky post lives in.
const POST_COLLECTION: &str = "app.bsky.feed.post";

/// Is there a byte-mover in this process: does anything provide
/// `post.create` (the atproto plugin, on the node it runs on)?
pub fn mover_available() -> bool {
    veles_core::services::available(proto::method::POST_CREATE)
}

/// What a node without a mover says.
pub const NO_MOVER: &str = "no public adapter in this process — /tcl replies are recorded and \
                            acted on by the node running the atproto plugin; answer there";

/// Bookkeeping for an APPROVE, before any repost: the exact text goes on
/// the allowlist (and off the blocked list — the owner's newest word
/// wins), and one stored proc is marked Approved (off the bad list).
/// Synchronous and local — no PDS needed.
pub fn approve_marks(entry: &WithheldEntry) -> Result<(), String> {
    let h = output_hash(&entry.output);
    safety::allowlist_add(&h)?;
    safety::block_remove(&h)?;
    // Each drawing too: the same drawing in another line goes out.
    for p in &entry.pictures {
        safety::allowlist_add(&p.text_hash)?;
        safety::block_remove(&p.text_hash)?;
    }
    if let Some(p) = &entry.proc_hash {
        safety::record(p, Verdict::Approved)?;
    }
    Ok(())
}

/// Bookkeeping for a DELETE: the output hash and the proc are marked
/// Blocked (never hosted again), and the text comes OFF the allowlist if
/// it was ever on it.
pub fn delete_marks(entry: &WithheldEntry) -> Result<(), String> {
    let h = output_hash(&entry.output);
    safety::block_add(&h)?;
    safety::allowlist_remove(&h)?;
    for p in &entry.pictures {
        safety::block_add(&p.text_hash)?;
        safety::allowlist_remove(&p.text_hash)?;
    }
    if let Some(p) = &entry.proc_hash {
        safety::record(p, Verdict::Blocked)?;
    }
    Ok(())
}

/// The rkey of an at-uri (`at://did/collection/rkey`).
fn rkey_of(uri: &str) -> Option<&str> {
    uri.rsplit('/').next().filter(|r| !r.is_empty())
}

/// `at://…/rkey#cid` → `at://…/rkey`: the request's at-uri rides in the
/// entry WITH its `#<cid>` fragment, and a fragment reaching the rkey
/// lookup 400s ("rkey must be a valid Record Key"; the 2026-10-06 live
/// round). The plugin strips it too; stripping here keeps the contract
/// on our side of the wire.
pub fn strip_cid_fragment(uri: &str) -> &str {
    uri.split_once('#').map_or(uri, |(u, _)| u)
}

/// The repost's thread target: the request when the entry carries one,
/// else the conversation root.
pub fn repost_target(entry: &WithheldEntry) -> String {
    let raw = if entry.request_msg_id.is_empty() {
        &entry.chat_id
    } else {
        &entry.request_msg_id
    };
    strip_cid_fragment(raw).to_string()
}

/// The text an approve reposts: the same an inline result posts (codes
/// stripped, blank lines trimmed, the first line's indentation kept, the
/// head of a long one), linking the hosted copy the withheld post already
/// made.
pub fn repost_text(entry: &WithheldEntry) -> String {
    let pt = safety::public_text(&entry.output);
    match entry.link.as_deref() {
        Some(link) => safety::inline_post(&pt, link),
        None if pt.elided => format!("{} …(truncated)", pt.body),
        None => pt.body,
    }
}

async fn call(service: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    if !veles_core::services::available(service) {
        return Err(NO_MOVER.to_string());
    }
    veles_core::services::call(service, params)
        .await
        .map_err(|e| format!("the plugin's {service} failed: {e}"))
}

/// Repost the approved output INLINE under the original request, then
/// take the withheld post(s) down: two bot posts for one answer is noise
/// (USER 2026-10-06). The short link and the pointer record stay — the
/// inline repost links to the same hosted copy.
async fn repost(entry: &WithheldEntry) -> Result<Acted, String> {
    let params = proto::PostCreateParams {
        text: repost_text(entry),
        reply_to: Some(repost_target(entry)),
        langs: vec!["en".to_string()],
    };
    let v = call(
        proto::method::POST_CREATE,
        serde_json::to_value(params).map_err(|e| e.to_string())?,
    )
    .await?;
    let made: proto::PostCreated = serde_json::from_value(v)
        .map_err(|e| format!("the plugin's post.create answer did not parse: {e}"))?;
    let removed = take_down_posts(&entry.post_uris).await;
    Ok(Acted {
        report: format!(
            "reposted inline under the request ({} post(s)), the withheld post(s) came down ({removed}); the text is allowlisted{}",
            made.uris.len(),
            if entry.proc_hash.is_some() {
                " and the proc is approved"
            } else {
                ""
            }
        ),
        posted: made.uris,
    })
}

/// Take these bot posts down; how many came down. A post that stays is
/// logged: it is still deletable by its link.
pub async fn take_down_posts(uris: &[String]) -> usize {
    let mut removed = 0;
    for uri in uris {
        let Some(rkey) = rkey_of(uri) else { continue };
        let p = proto::DeleteRecordParams {
            collection: POST_COLLECTION.into(),
            rkey: rkey.to_string(),
        };
        let Ok(params) = serde_json::to_value(p) else {
            continue;
        };
        match call(proto::method::REPO_DELETE_RECORD, params).await {
            Ok(_) => removed += 1,
            Err(e) => tracing::warn!("tcl: withheld post {uri} stayed after approve: {e}"),
        }
    }
    removed
}

/// Repost a reply with PICTURES as the reply itself would have gone out
/// (PLAN-slopdrop-art.md P4): its text, its drawings (the kept PNGs),
/// their self-labels and the rest of the code's spec, threaded under the
/// request, through the gateway's bus. The plugin's delivery receipt
/// lands on this entry (`ledger.entry`), and the receipt observer then
/// takes the withheld post down (`superseded`, set by [`act`]): the
/// reply is never gone while the repost is on its way.
///
/// A drawing nobody checked (anyone's, asked about first) is shown to
/// the image check now, so it carries the warning it calls for; the
/// owner approved it, so a check that fails does not stop the repost.
async fn repost_pictures(entry: &WithheldEntry) -> Result<Acted, String> {
    use serde_json::Value;
    let mut records: Vec<Value> = entry
        .spec
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let mut labels = entry.labels.clone();
    let mut unchecked = 0;
    let mut drawn = 0;
    let cfg = veles_core::config::load().ok();
    for p in &entry.pictures {
        let Some(png) = crate::art::kept_png(&p.sha) else {
            continue;
        };
        let path = crate::art::art_dir().join(format!("{}.png", p.sha));
        let seen = match &cfg {
            Some(c) => crate::art::classify_kept(c, &p.sha, &path).await.ok(),
            None => None,
        };
        match seen {
            Some(s) => {
                for l in s.labels() {
                    if !labels.iter().any(|x| x == l) {
                        labels.push(l.to_string());
                    }
                }
            }
            None => unchecked += 1,
        }
        records.push(crate::art::record_of(&png, &p.alt, p.width, p.height));
        drawn += 1;
    }
    if drawn == 0 {
        return Err(
            "the drawings are no longer kept (a week): delete it, or run the line again"
                .to_string(),
        );
    }
    records.extend(
        labels
            .iter()
            .map(|v| serde_json::json!({"kind": "label", "val": v})),
    );
    let mut meta = std::collections::BTreeMap::new();
    meta.insert(
        veles_types::bus::meta_keys::LEDGER_ENTRY.to_string(),
        entry.id.clone(),
    );
    meta.insert(
        veles_types::bus::meta_keys::BSKY_SPEC.to_string(),
        Value::Array(records).to_string(),
    );
    crate::bsky::send_reply(
        &entry.chat_id,
        &repost_target(entry),
        // A withheld long text keeps no post text: its head and link.
        entry
            .post_text
            .clone()
            .unwrap_or_else(|| repost_text(entry)),
        meta,
    )?;
    Ok(Acted {
        report: format!(
            "reposting with {drawn} picture(s){} under the request; the withheld post comes \
             down when the repost lands; the drawings are allowlisted{}",
            if labels.is_empty() {
                String::new()
            } else {
                format!(", labelled {}", labels.join(", "))
            },
            if unchecked > 0 {
                format!(" ({unchecked} not checked: the image check did not answer)")
            } else {
                String::new()
            }
        ),
        posted: Vec::new(),
    })
}

/// Take the post(s), the short link(s) and the pointer record(s) down.
async fn takedown(entry: &WithheldEntry) -> Result<Acted, String> {
    let mut notes: Vec<String> = Vec::new();
    let mut posts = 0;
    for uri in &entry.post_uris {
        let Some(rkey) = rkey_of(uri) else { continue };
        let p = proto::DeleteRecordParams {
            collection: POST_COLLECTION.into(),
            rkey: rkey.to_string(),
        };
        match call(
            proto::method::REPO_DELETE_RECORD,
            serde_json::to_value(p).map_err(|e| e.to_string())?,
        )
        .await
        {
            Ok(_) => posts += 1,
            Err(e) => {
                tracing::warn!("tcl: delete post {uri}: {e}");
                notes.push(format!("a post stayed up ({e})"));
            }
        }
    }
    let hosted = proto::HostedDeleteParams {
        short_ids: entry.short_ids.clone(),
        record_rkeys: entry.record_rkeys.clone(),
    };
    let done: proto::HostedDeleted =
        if hosted.short_ids.is_empty() && hosted.record_rkeys.is_empty() {
            proto::HostedDeleted::default()
        } else {
            let v = call(
                proto::method::HOSTED_DELETE,
                serde_json::to_value(hosted).map_err(|e| e.to_string())?,
            )
            .await?;
            serde_json::from_value(v)
                .map_err(|e| format!("the plugin's hosted.delete answer did not parse: {e}"))?
        };
    notes.extend(done.notes);
    let base = format!(
        "took down {posts} post(s), {} /t/ link(s) and {} pointer record(s); the PDS sweeps the blobs within ~7 h; the output and proc are blocked",
        done.short_links, done.records
    );
    Ok(Acted {
        report: if notes.is_empty() {
            base
        } else {
            format!("{base} — but: {}", notes.join("; "))
        },
        posted: Vec::new(),
    })
}

/// Act on a recorded entry, named by its id or anything the owner can see
/// of it (a post link, the short link, an rkey — `WithheldStore::find`):
/// the marks first, then the plugin moves bytes, then the store follows —
/// read fresh, by id, so nothing another answer changed in the meantime is
/// written back over. A plugin failure keeps the entry for a retry.
///
/// A delete removes the entry. An approve turns it INLINE and names the
/// repost, so the reposted output can still be deleted. An inline entry
/// has nothing to approve.
///
/// With NO mover in this process nothing is marked and nothing removed:
/// an approve that allowlisted the text but never reposted is a worse lie
/// than the reverse.
pub async fn act(target: &str, action: WithheldAction) -> Result<String, String> {
    let store = WithheldStore::load();
    let entry = store
        .find(target)
        .ok_or_else(|| format!("no recorded /tcl reply matches {target}"))?;
    let id = entry.id.clone();
    if action == WithheldAction::Approve && entry.kind == PostKind::Inline {
        return Err(format!(
            "{id} is already posted inline — nothing to approve"
        ));
    }
    if !mover_available() {
        return Err(NO_MOVER.to_string());
    }
    match action {
        WithheldAction::Approve => approve_marks(&entry)?,
        WithheldAction::Delete => delete_marks(&entry)?,
    }
    let pictures = !entry.pictures.is_empty();
    let acted = match action {
        WithheldAction::Approve if pictures => repost_pictures(&entry).await?,
        WithheldAction::Approve => repost(&entry).await?,
        WithheldAction::Delete => takedown(&entry).await?,
    };
    match action {
        WithheldAction::Delete => {
            let _ = store.remove(&id);
        }
        WithheldAction::Approve => {
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let posted = acted.posted.clone();
            if let Err(e) = store.update(&id, |e| {
                e.kind = PostKind::Inline;
                if pictures {
                    // The repost is on its way through the bus: its
                    // receipt adds its posts, then takes these down.
                    e.superseded = std::mem::take(&mut e.post_uris);
                } else {
                    // The text repost is up and the withheld posts are
                    // already down.
                    e.post_uris = posted;
                }
                e.reason = "approved by the owner".to_string();
                e.no_ask = false;
                e.at = at;
            }) {
                tracing::warn!("tcl: approved {id}, but recording the repost: {e}");
            }
        }
    }
    Ok(acted.report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repost_target_is_the_request_with_its_fragment_stripped() {
        let e = |msg: &str| WithheldEntry {
            id: "0123abcd".into(),
            chat_id: "at://did:plc:a/app.bsky.feed.post/root".into(),
            request_msg_id: msg.into(),
            output: "x".into(),
            ..Default::default()
        };
        assert_eq!(
            repost_target(&e("at://did:plc:a/app.bsky.feed.post/rk1#bafyreigabc")),
            "at://did:plc:a/app.bsky.feed.post/rk1"
        );
        let mut entry = e("");
        entry.chat_id = "at://did:plc:a/app.bsky.feed.post/root#cid".into();
        assert_eq!(
            repost_target(&entry),
            "at://did:plc:a/app.bsky.feed.post/root"
        );
        assert_eq!(
            rkey_of("at://did:plc:a/app.bsky.feed.post/rk1"),
            Some("rk1")
        );
    }

    /// Without a mover nothing is marked and the entry stays: the marks
    /// must never run ahead of a repost that cannot happen.
    #[tokio::test]
    async fn no_mover_marks_nothing_and_keeps_the_entry() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let ws = WithheldStore::load();
        ws.insert(WithheldEntry {
            id: "feedface".into(),
            kind: PostKind::Withheld,
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            output: "withheld text".into(),
            ..Default::default()
        })
        .unwrap();
        let err = act("feedface", WithheldAction::Approve).await.unwrap_err();
        assert_eq!(err, NO_MOVER);
        assert!(ws.get("feedface").is_some(), "kept for a retry");
        assert!(
            !safety::allowlisted(&output_hash("withheld text")),
            "not allowlisted without a repost"
        );
        let err = act("feedface", WithheldAction::Delete).await.unwrap_err();
        assert_eq!(err, NO_MOVER);
        assert!(!safety::blocked(&output_hash("withheld text")));
    }

    /// Approving a held picture reposts it AS a picture through the bus:
    /// the kept PNG with its alt text and labels, the rest of the code's
    /// spec, the text beside it, threaded under the request and naming its
    /// entry. The drawing is allowlisted. The withheld post comes down only
    /// when the repost's delivery receipt arrives, never before.
    #[tokio::test]
    async fn approving_a_held_picture_reposts_it_then_takes_the_note_down() {
        use std::sync::{Arc, Mutex};
        let (home, _env) = veles_core::testenv::scratch_home();
        // The plugin's services, stood in for.
        let deleted: Arc<Mutex<Vec<String>>> = Arc::default();
        let d = deleted.clone();
        let decl = |name: &str| veles_core::services::ServiceDecl {
            name: name.into(),
            description: String::new(),
            cap: None,
        };
        veles_core::services::register(
            decl(proto::method::POST_CREATE),
            veles_core::services::handler(|_p, _cx| async { Ok(serde_json::json!({"uris": []})) }),
        )
        .unwrap();
        veles_core::services::register(
            decl(proto::method::REPO_DELETE_RECORD),
            veles_core::services::handler(move |p, _cx| {
                d.lock()
                    .unwrap()
                    .push(p["rkey"].as_str().unwrap_or("").to_string());
                async { Ok(serde_json::json!({})) }
            }),
        )
        .unwrap();
        struct Withdraw;
        impl Drop for Withdraw {
            fn drop(&mut self) {
                veles_core::services::withdraw(proto::method::POST_CREATE);
                veles_core::services::withdraw(proto::method::REPO_DELETE_RECORD);
            }
        }
        let _w = Withdraw;
        // A gateway bus with a Bluesky adapter on it.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (itx, _irx) = tokio::sync::mpsc::unbounded_channel();
        let bus = veles_core::bus::GatewayBus {
            inbound: itx,
            outbound: Default::default(),
            last_chat: Default::default(),
            state_path: home.path().join("gateway-state.json"),
        };
        bus.register_outbound(crate::bsky::NETWORK, tx);
        crate::bsky::park_bus(bus);
        // A held drawing, already checked (no vision call here).
        let pic = crate::art::draw(&crate::art::Source {
            text: " ,__,  moo\n (oo)____\n (__)    )\\".into(),
            alt: String::new(),
            theme: veles_textart::Theme::Dark,
        })
        .unwrap();
        crate::art::remember(
            &pic.sha,
            &crate::art::Seen {
                categories: vec!["nudity".into()],
                ..Default::default()
            },
        );
        let root = "at://did:plc:op/app.bsky.feed.post/root";
        let quote = r#"[{"kind":"quote","uri":"at://did:plc:op/app.bsky.feed.post/par"}]"#;
        WithheldStore::load()
            .insert(WithheldEntry {
                id: "a11ce000".into(),
                kind: PostKind::Withheld,
                at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                chat_id: root.into(),
                request_msg_id: "at://did:plc:op/app.bsky.feed.post/ask#cask".into(),
                output: "the cow".into(),
                pictures: vec![pic.reference("a cow says moo")],
                spec: Some(quote.into()),
                post_text: Some("(state: proc cow changed)".into()),
                post_uris: vec!["at://did:plc:bot/app.bsky.feed.post/held1".into()],
                ..Default::default()
            })
            .unwrap();

        let report = act("a11ce000", WithheldAction::Approve).await.unwrap();
        assert!(report.contains("1 picture(s), labelled nudity"), "{report}");
        // The bus forwards through its redaction task.
        let o = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .ok()
            .flatten()
            .expect("the repost went through the bus");
        assert_eq!(o.chat_id, root);
        assert_eq!(
            o.reply_to.as_deref(),
            Some("at://did:plc:op/app.bsky.feed.post/ask")
        );
        assert_eq!(o.text, "(state: proc cow changed)");
        use veles_types::bus::meta_keys;
        assert_eq!(
            o.meta.get(meta_keys::LEDGER_ENTRY).map(String::as_str),
            Some("a11ce000")
        );
        let spec: serde_json::Value =
            serde_json::from_str(o.meta.get(meta_keys::BSKY_SPEC).unwrap()).unwrap();
        let kinds: Vec<&str> = spec
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["quote", "art", "label"]);
        assert_eq!(spec[1]["alt"], "a cow says moo");
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(spec[1]["png"].as_str().unwrap())
            .unwrap();
        assert_eq!(png, pic.png, "the kept PNG, as drawn");
        assert_eq!(spec[2]["val"], "nudity");
        assert!(
            safety::allowlisted(&pic.text_hash),
            "the drawing is approved"
        );
        let e = WithheldStore::load().get("a11ce000").unwrap();
        assert_eq!(e.kind, PostKind::Inline);
        assert!(e.post_uris.is_empty());
        assert_eq!(e.superseded, ["at://did:plc:bot/app.bsky.feed.post/held1"]);
        assert!(deleted.lock().unwrap().is_empty(), "nothing down yet");

        // The repost's receipt: its post is recorded, then the note goes.
        use veles_core::bus::DeliveryObserver as _;
        crate::receipts::Observer.delivered(
            &o.meta,
            &["at://did:plc:bot/app.bsky.feed.post/new1".to_string()],
            &Default::default(),
        );
        for _ in 0..100 {
            if !deleted.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(*deleted.lock().unwrap(), ["held1"]);
        let e = WithheldStore::load().get("a11ce000").unwrap();
        assert_eq!(e.post_uris, ["at://did:plc:bot/app.bsky.feed.post/new1"]);
        assert!(e.superseded.is_empty());

        // A withheld long text keeps its whole text drawn and no post text
        // of its own: the approve reposts the head and the link, in one
        // post, with the picture.
        let long: String = (0..40)
            .map(|i| format!("line {i:02} of a long answer\n"))
            .collect();
        let page = crate::art::draw_text(&long).unwrap();
        crate::art::remember(&page.sha, &crate::art::Seen::default());
        let link = "https://7778777.online/t/eiaaaaaaaaaaaaaa";
        WithheldStore::load()
            .insert(WithheldEntry {
                id: "10de0000".into(),
                kind: PostKind::Withheld,
                at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                chat_id: root.into(),
                request_msg_id: "at://did:plc:op/app.bsky.feed.post/ask2".into(),
                output: long.clone(),
                link: Some(link.into()),
                pictures: vec![page.reference(&page.alt)],
                post_text: None,
                post_uris: vec!["at://did:plc:bot/app.bsky.feed.post/held2".into()],
                ..Default::default()
            })
            .unwrap();
        act("10de0000", WithheldAction::Approve).await.unwrap();
        let o = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .ok()
            .flatten()
            .expect("the repost went through the bus");
        assert!(o.text.starts_with("line 00 of a long answer"), "{}", o.text);
        assert!(
            o.text
                .ends_with(&format!("…(truncated — full output: {link})")),
            "{}",
            o.text
        );
        assert!(
            o.text.chars().count() <= 300,
            "the head and the link in one post: {}",
            o.text.chars().count()
        );
        let spec: serde_json::Value =
            serde_json::from_str(o.meta.get(meta_keys::BSKY_SPEC).unwrap()).unwrap();
        assert_eq!(spec[0]["kind"], "art", "{spec}");
    }
}
