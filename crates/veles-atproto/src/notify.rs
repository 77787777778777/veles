//! The authoritative notification lane: poll `listNotifications` on our
//! OWN PDS, emit matched mention/reply events, advance the seen mark.
//!
//! This is the durable backstop the Jetstream hint lane reconciles
//! against. Jetstream can miss (reconnect gaps, lag, a dropped frame);
//! the AppView's notification table cannot — every mention/reply is a
//! row there until we mark it seen. `is_read`/seen is the free resume.
//!
//! Events emitted here flow through the SAME gateway dedup as Jetstream
//! matches, so a mention seen on both lanes is handled exactly once.

use anyhow::Result;

use crate::event::Lane;
use crate::filter::{FilterHandle, MatchedEvent};
use crate::pds::PdsClient;

/// The reasons that are turns: a mention or a reply of the bot.
pub const TURN_REASONS: &[&str] = &["mention", "reply"];

/// The reasons only the trigger engine reads. They are asked for in a
/// list of their own, so a burst of likes cannot push a mention off the
/// turn lane's page. Until 2026-10-11 the poll asked for the turn
/// reasons alone, so no LIKE, REPOST, FOLLOW or QUOTE trigger could fire.
pub const NOTICE_REASONS: &[&str] = &["like", "repost", "follow", "quote"];

/// A notification the trigger engine may act on (Bluesky event triggers,
/// USER 2026-10-10): a like, a repost, a follow, a quote, a reply or a
/// mention of the bot. Separate from the turn lane: a mention is ALSO a
/// turn, and a trigger answering it is a second, deliberate reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// LIKE | REPOST | FOLLOW | QUOTE | REPLY | MENTION.
    pub event: String,
    pub did: String,
    pub handle: String,
    /// The actor's record (the like, the repost, the follow, the post).
    pub uri: String,
    pub cid: String,
    /// The bot's post it is about (like, repost, quote).
    pub subject: String,
    /// The actor's post text (quote, reply, mention).
    pub text: String,
    /// The thread an answer belongs in.
    pub thread: String,
}

/// Map one notification to a [`Notice`]; `None` for a reason the trigger
/// engine does not bind, or the bot's own doing.
pub fn notification_to_notice(n: &serde_json::Value, bot_did: &str) -> Option<Notice> {
    let s =
        |v: Option<&serde_json::Value>| v.and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let event = match n.get("reason").and_then(|r| r.as_str())? {
        "like" => "LIKE",
        "repost" => "REPOST",
        "follow" => "FOLLOW",
        "quote" => "QUOTE",
        "reply" => "REPLY",
        "mention" => "MENTION",
        _ => return None,
    };
    let did = s(n.get("author").and_then(|a| a.get("did")));
    if did.is_empty() || did == bot_did {
        return None;
    }
    let record = n.get("record").unwrap_or(&serde_json::Value::Null);
    let uri = s(n.get("uri"));
    let subject = s(n.get("reasonSubject"));
    let has_post = matches!(event, "QUOTE" | "REPLY" | "MENTION");
    let thread = if has_post {
        let root = s(record
            .get("reply")
            .and_then(|r| r.get("root"))
            .and_then(|r| r.get("uri")));
        if root.is_empty() {
            uri.clone()
        } else {
            root
        }
    } else {
        subject.clone()
    };
    Some(Notice {
        event: event.to_string(),
        did,
        handle: s(n.get("author").and_then(|a| a.get("handle"))),
        uri,
        cid: s(n.get("cid")),
        subject,
        text: if has_post {
            s(record.get("text"))
        } else {
            String::new()
        },
        thread,
    })
}

/// Run the notification poll forever, on `period`. Returns on shutdown.
/// `notices`, when given, receives every unread notification the trigger
/// engine binds ([`Notice`]).
pub async fn run_notification_poll(
    client: PdsClient,
    bot_did: String,
    handle: FilterHandle,
    period: std::time::Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    notices: Option<tokio::sync::mpsc::UnboundedSender<Notice>>,
) -> Result<()> {
    tracing::info!(
        "atproto notification poll: every {}s on our own PDS",
        period.as_secs()
    );
    // Warn once per outage, not every tick: a misconfigured PDS logged
    // the same 400 every 30 s (1,362 times) and buried everything else.
    let mut failing = false;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        match poll_once(&client, &bot_did, &handle, notices.as_ref()).await {
            Ok(()) if failing => {
                failing = false;
                tracing::info!("atproto notification poll: working again");
            }
            Ok(()) => {}
            // Transient or not, never fatal: ride the next tick.
            Err(e) if !failing => {
                failing = true;
                tracing::warn!(
                    "atproto notification poll: {e:#} (said once; retrying every {}s)",
                    period.as_secs()
                );
            }
            Err(e) => tracing::debug!("atproto notification poll: {e:#}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(period) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return Ok(()); }
            }
        }
    }
}

/// One poll: take the timestamp BEFORE listing (so anything indexed
/// mid-loop stays unread for the next pass), emit unread mention/reply
/// notifications as turns and every bound reason as a [`Notice`], then
/// advance the seen mark to that timestamp.
async fn poll_once(
    client: &PdsClient,
    bot_did: &str,
    handle: &FilterHandle,
    notices: Option<&tokio::sync::mpsc::UnboundedSender<Notice>>,
) -> Result<()> {
    let seen_at = iso_now();
    let mut lists = vec![TURN_REASONS];
    if notices.is_some() {
        lists.push(NOTICE_REASONS);
    }
    let mut notifs = Vec::new();
    for reasons in lists {
        let out = client.list_notifications(reasons, 50).await?;
        let Some(page) = out.get("notifications").and_then(|n| n.as_array()) else {
            return Ok(());
        };
        notifs.extend(page.iter().cloned());
    }
    let mut emitted = 0usize;
    for n in &notifs {
        // Only act on unread ones; the seen mark handles the rest.
        if n.get("isRead").and_then(|r| r.as_bool()).unwrap_or(false) {
            continue;
        }
        if let (Some(tx), Some(notice)) = (notices, notification_to_notice(n, bot_did)) {
            let _ = tx.send(notice);
        }
        let reason = n.get("reason").and_then(|r| r.as_str()).unwrap_or("");
        if reason != "mention" && reason != "reply" {
            continue;
        }
        if let Some(ev) = notification_to_event(n, bot_did) {
            handle.publish(ev);
            emitted += 1;
        }
    }
    if emitted > 0 {
        tracing::info!("atproto notification poll: {emitted} unread mention/reply");
    }
    // Advance the mark to the pre-list timestamp regardless of whether we
    // emitted: a notification we deliberately skip (a reason we don't
    // handle, a non-post) should not be re-read forever.
    client.update_seen(&seen_at).await?;
    Ok(())
}

/// Map one notification to a MatchedEvent, through the mention-gate.
/// None = not a gated post (a reason we skip, a non-post record, a post
/// not addressed to the bot).
pub fn notification_to_event(n: &serde_json::Value, bot_did: &str) -> Option<MatchedEvent> {
    let uri = n.get("uri").and_then(|u| u.as_str())?;
    let (repo, collection, rkey) = crate::post::parse_at_uri(uri).ok()?;
    if collection != "app.bsky.feed.post" {
        return None;
    }
    let record = n.get("record").cloned().unwrap_or(serde_json::Value::Null);
    // The gate is the authority — a notification for a reason we asked
    // about still must pass it (belt and braces against a reason we did
    // not anticipate being bucketed under mention/reply by the AppView).
    let author_did = n
        .get("author")
        .and_then(|a| a.get("did"))
        .and_then(|d| d.as_str())
        .unwrap_or(&repo)
        .to_string();
    // The bot's own posts are never its turns (see `gate_bot_post`).
    if crate::event::gate_bot_post(bot_did, &author_did, &record) != crate::event::Gate::Addressed {
        return None;
    }
    let author_handle = n
        .get("author")
        .and_then(|a| a.get("handle"))
        .and_then(|h| h.as_str())
        .map(str::to_string);
    let cid = n.get("cid").and_then(|c| c.as_str()).map(str::to_string);
    let text = record
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let commit_ev = crate::event::CommitEvent {
        did: author_did.clone(),
        collection,
        rkey,
        operation: "create".into(),
        record: Some(record.clone()),
        cid,
        seq: None,
    };
    Some(MatchedEvent {
        lane: Lane::Notification,
        key: crate::event::event_key(&commit_ev),
        thread: crate::event::thread_key(&commit_ev),
        author_did,
        author_handle,
        text,
        post_uri: uri.to_string(),
        record,
        cursor: None,
        at_us: None,
    })
}

/// RFC 3339 UTC `Z`, for the seen mark.
fn iso_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    crate::post::format_epoch_pub(d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BOT: &str = "did:plc:bot";

    fn notif(reason: &str, read: bool, mention_did: &str) -> serde_json::Value {
        json!({
            "uri": "at://did:plc:alice/app.bsky.feed.post/rk1",
            "cid": "bafy",
            "author": { "did": "did:plc:alice", "handle": "alice.bsky.social" },
            "reason": reason,
            "isRead": read,
            "record": { "$type":"app.bsky.feed.post", "text":"hi @bot",
                "facets":[{"index":{"byteStart":0,"byteEnd":3},
                    "features":[{"$type":"app.bsky.richtext.facet#mention","did":mention_did}]}] },
            "indexedAt": "2026-10-05T00:00:00Z"
        })
    }

    #[test]
    fn an_unread_mention_notification_becomes_an_event() {
        let ev = notification_to_event(&notif("mention", false, BOT), BOT).unwrap();
        assert_eq!(ev.lane, Lane::Notification);
        assert_eq!(ev.author_did, "did:plc:alice");
        assert_eq!(ev.author_handle.as_deref(), Some("alice.bsky.social"));
        assert_eq!(ev.post_uri, "at://did:plc:alice/app.bsky.feed.post/rk1");
        // The dedup key matches the Jetstream key for the same post — that
        // is what makes the two lanes reconcile in the gateway's ledger.
        assert_eq!(ev.key, "at://did:plc:alice/app.bsky.feed.post/rk1#bafy");
    }

    #[test]
    fn a_notification_for_the_bots_own_post_is_dropped() {
        let mut n = notif("reply", false, BOT);
        n["author"] = json!({ "did": BOT, "handle": "bot.example" });
        n["uri"] = json!(format!("at://{BOT}/app.bsky.feed.post/rk2"));
        assert!(notification_to_event(&n, BOT).is_none());
    }

    #[test]
    fn a_notification_for_someone_elses_mention_is_dropped() {
        assert!(notification_to_event(&notif("mention", false, "did:plc:other"), BOT).is_none());
    }

    #[test]
    fn every_bound_reason_becomes_a_notice_with_its_subject_and_thread() {
        let like = json!({"uri": "at://did:plc:a/app.bsky.feed.like/k", "cid": "c",
            "author": {"did": "did:plc:a", "handle": "a.test"}, "reason": "like",
            "reasonSubject": "at://did:plc:bot/app.bsky.feed.post/p", "isRead": false, "record": {}});
        let n = notification_to_notice(&like, BOT).unwrap();
        assert_eq!(n.event, "LIKE");
        assert_eq!(n.subject, "at://did:plc:bot/app.bsky.feed.post/p");
        assert_eq!(n.thread, n.subject);
        assert_eq!(n.text, "");
        let mut reply = notif("reply", false, BOT);
        reply["record"]["reply"] = json!({"root": {"uri": "at://root", "cid": "r"}, "parent": {"uri": "at://p", "cid": "p"}});
        let n = notification_to_notice(&reply, BOT).unwrap();
        assert_eq!(
            (n.event.as_str(), n.thread.as_str(), n.text.as_str()),
            ("REPLY", "at://root", "hi @bot")
        );
        let follow = json!({"uri": "at://did:plc:a/app.bsky.graph.follow/f", "cid": "c",
            "author": {"did": "did:plc:a"}, "reason": "follow", "isRead": false, "record": {}});
        assert_eq!(
            notification_to_notice(&follow, BOT).unwrap().event,
            "FOLLOW"
        );
        let mut own = like.clone();
        own["author"] = json!({"did": BOT});
        assert!(
            notification_to_notice(&own, BOT).is_none(),
            "the bot's own doing"
        );
        let mut other = like;
        other["reason"] = json!("starterpack-joined");
        assert!(notification_to_notice(&other, BOT).is_none());
    }

    /// A stub PDS that answers `listNotifications` by the reasons asked
    /// for, the way the AppView does: a reason nobody asks for is never
    /// returned.
    async fn stub_pds(asked: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>) -> String {
        use axum::{extract::RawQuery, routing::get, routing::post, Json, Router};
        let app = Router::new()
            .route(
                "/xrpc/com.atproto.server.createSession",
                post(|| async {
                    Json(json!({"accessJwt":"a","refreshJwt":"r",
                        "did": BOT, "handle":"bot.example"}))
                }),
            )
            .route(
                "/xrpc/app.bsky.notification.updateSeen",
                post(|| async { axum::http::StatusCode::OK }),
            )
            .route(
                "/xrpc/app.bsky.notification.listNotifications",
                get(move |RawQuery(q): RawQuery| {
                    let reasons: Vec<String> = q
                        .unwrap_or_default()
                        .split('&')
                        .filter_map(|kv| kv.strip_prefix("reasons="))
                        .map(str::to_string)
                        .collect();
                    asked.lock().unwrap().push(reasons.clone());
                    let like = json!({"uri": "at://did:plc:a/app.bsky.feed.like/k", "cid": "c",
                        "author": {"did": "did:plc:a", "handle": "a.test"}, "reason": "like",
                        "reasonSubject": "at://did:plc:bot/app.bsky.feed.post/p",
                        "isRead": false, "record": {}});
                    let all = [notif("mention", false, BOT), like];
                    let page: Vec<_> = all
                        .into_iter()
                        .filter(|n| reasons.iter().any(|r| n["reason"] == r.as_str()))
                        .collect();
                    async move { Json(json!({ "notifications": page })) }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        url
    }

    /// The poll asks for every reason the trigger engine binds, not only
    /// the turn reasons: a like reaches the notice channel, and the
    /// mention is still a turn. Asking for mention and reply alone (the
    /// lane before 2026-10-11) left every LIKE trigger silent.
    #[tokio::test]
    async fn a_like_reaches_the_trigger_engine_and_a_mention_stays_a_turn() {
        let asked = std::sync::Arc::default();
        let url = stub_pds(std::sync::Arc::clone(&asked)).await;
        let client = PdsClient::login(&url, &url, "bot.example", "pw")
            .await
            .unwrap();
        let (filter, handle) = crate::filter::Filter::new(8);
        let mut turns = filter.subscribe();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        poll_once(&client, BOT, &handle, Some(&tx)).await.unwrap();
        let mut events = Vec::new();
        while let Ok(n) = rx.try_recv() {
            events.push(n.event);
        }
        events.sort();
        assert_eq!(events, ["LIKE", "MENTION"]);
        let turn = turns.try_recv().expect("the mention is a turn");
        assert_eq!(turn.post_uri, "at://did:plc:alice/app.bsky.feed.post/rk1");
        assert!(turns.try_recv().is_err(), "a like is never a turn");

        // With no trigger engine listening, only the turn reasons are read.
        asked.lock().unwrap().clear();
        poll_once(&client, BOT, &handle, None).await.unwrap();
        assert_eq!(*asked.lock().unwrap(), [["mention", "reply"]]);
    }

    #[test]
    fn a_non_post_notification_is_dropped() {
        let n = json!({ "uri":"at://did:plc:a/app.bsky.feed.like/k", "cid":"c",
            "author":{"did":"did:plc:a"}, "reason":"like", "isRead":false, "record":{} });
        assert!(notification_to_event(&n, BOT).is_none());
    }
}
