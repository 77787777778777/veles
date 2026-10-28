//! The shared event filter: owns the upstream lanes and fans matched
//! events out to consumers. Lanes:
//!
//! - **Jetstream v2** (`run_jetstream`, `network.bsky.jetstream.subscribeEvents`
//!   on `jetstream.us-east.bsky.network`): the network-wide HINT lane. Plain
//!   JSON, server-side collection filter, `seq` cursor-resume, replay-dedup.
//!   (Until 2026-10-06 this lane spoke legacy v1 — `jetstream2.…/subscribe`,
//!   where "2" names the second v1 host, not a version.)
//!   NO signatures — a matched event only ever triggers the consumer's
//!   own authenticated re-fetch / notification reconcile; it never authors
//!   a turn by itself.
//! - **Local firehose** (the co-located vlpds `subscribeRepos`):
//!   authoritative for repos WE host. Lands beside Jetstream; same fan-out.
//!
//! The match engine is declarative per consumer: a consumer registers the
//! set of DIDs it cares about and the gate decides "addressed to me".
//! veles registers the bot DID (mention-gate); courier registers the DIDs
//! it pushes for (any app-notif record touching them).

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::event::{CommitEvent, Gate, Lane};

/// A matched event handed to consumers. The event is the parsed commit;
/// `lane` says how much to trust it; `key` is the stable dedup id.
#[derive(Debug, Clone)]
pub struct MatchedEvent {
    pub lane: Lane,
    pub key: String,
    /// The thread this belongs in (the conversation's chat_id).
    pub thread: String,
    /// The author's DID (`atproto:<did>` is the sender_id).
    pub author_did: String,
    /// The author's handle, when the event carried one (display only).
    pub author_handle: Option<String>,
    /// The post's text (the turn's input), when it is a post.
    pub text: String,
    /// The at-uri of THIS post (the reply target's parent).
    pub post_uri: String,
    /// The raw record, for a consumer that needs more than the text.
    pub record: serde_json::Value,
    /// The lane's cursor value at this event, for resume persistence
    /// (Jetstream v2: the event's `seq`).
    pub cursor: Option<i64>,
    /// When the event was committed, in microseconds since the epoch (v2:
    /// the event's `time`). The latency log's reference point; the cursor
    /// is a sequence number, not a time.
    pub at_us: Option<i64>,
}

/// A consumer subscription: which DIDs it cares about, and the channel
/// matched events arrive on. `broadcast` so several consumers each see
/// every match; each keeps its OWN cursor + dedup downstream.
#[derive(Clone)]
pub struct Filter {
    tx: broadcast::Sender<MatchedEvent>,
}

impl Filter {
    /// Start the filter with the given capacity. Lanes are spawned by the
    /// caller and handed a `FilterHandle` to publish into.
    pub fn new(capacity: usize) -> (Self, FilterHandle) {
        let (tx, _) = broadcast::channel(capacity);
        (Self { tx: tx.clone() }, FilterHandle { tx })
    }

    /// Subscribe a consumer with NO gate: every parsed event arrives.
    /// This is courier's shape — it registers the DIDs it pushes for and
    /// matches them itself. Lagging consumers get `RecvError::Lagged` and
    /// reconcile from their own cursor / the notification poll.
    pub fn subscribe(&self) -> broadcast::Receiver<MatchedEvent> {
        self.tx.subscribe()
    }
}

/// The publishing half lanes hold.
#[derive(Clone)]
pub struct FilterHandle {
    tx: broadcast::Sender<MatchedEvent>,
}

impl FilterHandle {
    /// Publish a matched event. A send with no subscribers is fine (the
    /// filter may run before any consumer attaches).
    pub fn publish(&self, ev: MatchedEvent) {
        let _ = self.tx.send(ev);
    }
}

/// The Jetstream v2 frame: `{"$type":"message","payload":{…}}`, where the
/// payload's `$type` ends in `#commit`, `#account`, `#identity`, `#sync` or
/// `#info` (a subset is read; serde ignores the rest).
#[derive(Debug, serde::Deserialize)]
struct JetstreamFrame {
    #[serde(default)]
    payload: Option<JetstreamPayload>,
}

#[derive(Debug, serde::Deserialize)]
struct JetstreamPayload {
    #[serde(rename = "$type", default)]
    kind: String,
    #[serde(default)]
    did: Option<String>,
    /// The resume cursor (inclusive: a reconnect at `seq` gets it again).
    #[serde(default)]
    seq: Option<i64>,
    /// RFC 3339, microseconds.
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    collection: Option<String>,
    #[serde(default)]
    rkey: Option<String>,
    #[serde(default)]
    record: Option<serde_json::Value>,
    #[serde(default)]
    cid: Option<String>,
    /// `#info` frames: e.g. `OutdatedCursor`.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// One frame, classified.
#[derive(Debug)]
pub enum Frame {
    /// An app-notification create: what consumers get.
    Event(Box<MatchedEvent>),
    /// The server's own notice. `OutdatedCursor` means the saved cursor was
    /// older than the server's retention, and the stream starts at its floor.
    Info { name: String, message: String },
    /// Anything else: updates, deletes, other collections, account and
    /// identity events, unparseable text.
    Skip,
}

/// Run the Jetstream v2 lane, reconnecting forever, publishing every
/// parsed app-notif event to the filter (consumers gate downstream).
/// `cursor_store` persists the resume cursor. Returns only on a fatal
/// error the reconnect loop cannot survive.
///
/// This lane is a HINT: it never authors a turn. The consumer (adapter)
/// reconciles matched events against the authoritative notification poll
/// before acting.
pub async fn run_jetstream(
    url: &str,
    collections: &[String],
    handle: FilterHandle,
    cursor_store: Arc<CursorStore>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut backoff = BackoffMs(500);
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let resumed = cursor_store.load().ok().flatten().is_some();
        let (end, progressed) =
            match jetstream_once(url, collections, &handle, &cursor_store, &mut shutdown).await {
                Ok(progressed) => (Ok(()), progressed),
                Err(e) => (Err(e), false),
            };
        // A resume that delivered nothing is a dead end, and the server
        // does not always say so: a cursor below its retention floor (or
        // one it simply will not serve — 2026-10-09, ~10:00 on, 60–80
        // times an hour) completes the handshake and then sends NOTHING,
        // no frames and no OutdatedCursor notice, so the stall watchdog
        // fires, the same cursor is offered again, and the lane loops
        // forever while reporting itself online. Dropping the cursor
        // makes the next connect live, which always works; the gap is
        // the notification poll's to cover, as a cold start's is. (The
        // same fix courier needed the same day.)
        if resumed && !progressed && !*shutdown.borrow() {
            tracing::warn!(
                "atproto jetstream: a resumed connection delivered no commit; \
                 dropping the cursor and connecting live"
            );
            cursor_store.forget();
        }
        match end {
            // A clean end is a shutdown only when one was asked for. The
            // server closing the socket also ends cleanly, and used to end
            // the lane for good: mentions then waited on the notification
            // poll until the gateway restarted.
            Ok(()) if *shutdown.borrow() => return Ok(()),
            Ok(()) => {
                tracing::warn!("atproto jetstream: closed by the server; reconnecting");
                backoff = BackoffMs(500);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(backoff.0)) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("atproto jetstream: {e:#}; reconnecting in {}ms", backoff.0);
                backoff = BackoffMs((backoff.0 * 2).min(30_000));
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(backoff.0)) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    }
                }
            }
        }
    }
}

struct BackoffMs(u64);

/// One connection's life. Answers whether at least one commit was
/// parsed on it (`true`): the reconnect loop drops a resume cursor that
/// a whole session delivered nothing on.
async fn jetstream_once(
    url: &str,
    collections: &[String],
    handle: &FilterHandle,
    cursor_store: &Arc<CursorStore>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<bool> {
    let connect = subscribe_url(url, collections, cursor_store.load()?);
    tracing::info!("atproto jetstream: connecting {connect}");
    let (ws, _) = tokio_tungstenite::connect_async(&connect)
        .await
        .context("jetstream connect")?;
    let (mut write, mut read) = ws.split();
    tracing::info!("atproto jetstream: online");

    // DECOUPLE the socket drain from processing (USER 2026-10-05: "parse
    // those events a lot faster if I'm going to use this for notifications
    // later"). The old loop was strictly sequential: read one frame, parse
    // it (sync, on the async worker), save the cursor (sync mutex + a disk
    // write every 100th, on the async worker), publish — THEN read the
    // next. On a small host a busy worker means the socket isn't drained,
    // TCP backpressure builds, the server's send buffer fills, and it
    // RESETS the slow consumer — the every-few-minutes "Connection reset"
    // dead windows that swallowed mentions.
    //
    // Now a dedicated reader task does nothing but read.next() and ship
    // the frame into a channel as fast as the kernel delivers — it never
    // parses, never touches the disk, never publishes. The processing loop
    // below drains that channel. A bounded channel gives backpressure we
    // can SEE (a full channel = we're falling behind, log it) instead of
    // silently buffering in the kernel until the server drops us.
    let (frame_tx, mut frame_rx) =
        tokio::sync::mpsc::channel::<tokio_tungstenite::tungstenite::Message>(4096);
    let mut read_shutdown = shutdown.clone();
    let reader = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            return Err(anyhow::anyhow!("jetstream frame: {e}"));
                        }
                        None => return Ok(()), // stream ended
                    };
                    match msg {
                        tokio_tungstenite::tungstenite::Message::Ping(p) => {
                            // Answer keepalives right here in the reader so
                            // a processing backlog never delays a pong.
                            let _ = write.send(tokio_tungstenite::tungstenite::Message::Pong(p)).await;
                        }
                        tokio_tungstenite::tungstenite::Message::Close(_) => {
                            return Ok(());
                        }
                        other => {
                            // Ship the frame; if the channel is full we are
                            // falling behind — that is the signal to act on,
                            // not a silent kernel buffer.
                            if frame_tx.send(other).await.is_err() {
                                return Ok(()); // processor gone
                            }
                        }
                    }
                }
                _ = read_shutdown.changed() => {
                    if *read_shutdown.borrow() { return Ok(()); }
                }
            }
        }
    });

    // A stall watchdog keyed on CURSOR PROGRESS, not frame arrival. The
    // app-notif set commits every few ms network-wide, so a cursor that has
    // not advanced in 15s means the socket is dead-to-us regardless of
    // keepalive pings. On stall, force a reconnect — it resumes from the
    // last cursor and replays the gap.
    const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    let mut progressed = false;
    let mut last_progress = tokio::time::Instant::now();
    let mut backlog_watch = BacklogWatch::default();
    loop {
        tokio::select! {
            frame = frame_rx.recv() => {
                let Some(msg) = frame else { break }; // reader ended
                let text = match msg {
                    tokio_tungstenite::tungstenite::Message::Text(t) => t,
                    tokio_tungstenite::tungstenite::Message::Binary(b) => {
                        // Jetstream v2 is JSON text; a binary frame is
                        // zstd-dict compressed, which we do not negotiate.
                        tracing::debug!("atproto jetstream: binary frame skipped ({} bytes)", b.len());
                        continue;
                    }
                    _ => continue, // pings/pongs handled in the reader
                };
                let matched = match parse_frame(&text) {
                    Frame::Event(matched) => *matched,
                    Frame::Info { name, message } => {
                        tracing::warn!("atproto jetstream: {name}: {message}");
                        continue;
                    }
                    Frame::Skip => continue,
                };
                // A real commit advanced the stream: that is liveness.
                last_progress = tokio::time::Instant::now();
                progressed = true;
                if let Some(cur) = matched.cursor {
                    cursor_store.save(cur);
                }
                handle.publish(matched);
                // Surface backpressure: if the channel is backing up we are
                // NOT keeping up — log so it's visible, not a silent reset.
                match backlog_watch.observe(frame_rx.len(), std::time::Instant::now()) {
                    Some(BacklogLine::Behind(line)) => tracing::warn!("{line}"),
                    Some(BacklogLine::CaughtUp(line)) => tracing::info!("{line}"),
                    None => {}
                }
            }
            _ = tokio::time::sleep(STALL_TIMEOUT) => {
                if last_progress.elapsed() >= STALL_TIMEOUT {
                    tracing::warn!(
                        "atproto jetstream: no commit in {}s — stalled socket, reconnecting",
                        STALL_TIMEOUT.as_secs()
                    );
                    // Stop the reader before bailing so it does not leak
                    // detached (the select! bail skips the teardown below).
                    reader.abort();
                    anyhow::bail!("jetstream stalled (no commit in {}s)", STALL_TIMEOUT.as_secs());
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    // The processing loop broke (shutdown or the reader ended). Wait for
    // the reader to finish and propagate any fatal read error. The reader
    // has its own shutdown clone, so a shutdown signal ends it too; if the
    // reader ended on its own (stream closed), the channel closed and we
    // broke out above.
    match reader.await {
        Ok(Ok(())) => Ok(progressed),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("reader task: {e}")),
    }
}

/// The subscribe URL: one `collections=` per collection (v2's name for
/// v1's `wantedCollections`) and the resume cursor, if there is one.
pub fn subscribe_url(base: &str, collections: &[String], cursor: Option<i64>) -> String {
    let mut connect = base.to_string();
    let mut sep = if connect.contains('?') { '&' } else { '?' };
    for c in collections {
        connect.push_str(&format!("{sep}collections={c}"));
        sep = '&';
    }
    if let Some(cur) = cursor {
        connect.push_str(&format!("{sep}cursor={cur}"));
    }
    connect
}

/// Parse one Jetstream text frame into a MatchedEvent — BROADLY, with no
/// consumer gate. The lane publishes every app-notification commit; each
/// consumer applies its own gate downstream (veles: bot-mention).
/// None = not an event a consumer could care about (an account/identity/
/// sync/info frame, a non-create op, or a collection outside the app-notif
/// set).
pub fn parse_event(text: &str) -> Option<MatchedEvent> {
    match parse_frame(text) {
        Frame::Event(ev) => Some(*ev),
        Frame::Info { .. } | Frame::Skip => None,
    }
}

/// Classify one Jetstream v2 frame.
pub fn parse_frame(text: &str) -> Frame {
    let Some(payload) = serde_json::from_str::<JetstreamFrame>(text)
        .ok()
        .and_then(|f| f.payload)
    else {
        return Frame::Skip;
    };
    match payload.kind.rsplit('#').next().unwrap_or_default() {
        "commit" => {}
        "info" => {
            return Frame::Info {
                name: payload.name.unwrap_or_default(),
                message: payload.message.unwrap_or_default(),
            }
        }
        _ => return Frame::Skip,
    }
    if payload.operation.as_deref() != Some("create") {
        return Frame::Skip; // updates/deletes never author a new event
    }
    let collection = payload.collection.unwrap_or_default();
    // The app-notification set, plus posts (which carry mentions/replies):
    // the union any consumer gates within.
    const APP_NOTIF: &[&str] = &[
        "app.bsky.feed.post",
        "app.bsky.feed.like",
        "app.bsky.feed.repost",
        "app.bsky.graph.follow",
    ];
    if !APP_NOTIF.contains(&collection.as_str()) {
        return Frame::Skip;
    }
    let record = payload.record.unwrap_or(serde_json::Value::Null);
    let author_did = payload.did.unwrap_or_default();
    let rkey = payload.rkey.unwrap_or_default();
    let cid = payload.cid.filter(|c| !c.is_empty());
    let post_uri = format!("at://{author_did}/{collection}/{rkey}");
    let commit_ev = CommitEvent {
        did: author_did.clone(),
        collection,
        rkey,
        operation: "create".into(),
        record: Some(record.clone()),
        cid,
        seq: payload.seq,
    };
    let text_body = record
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let at_us = payload
        .time
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp_micros());
    Frame::Event(Box::new(MatchedEvent {
        lane: Lane::Jetstream,
        key: crate::event::event_key(&commit_ev),
        thread: crate::event::thread_key(&commit_ev),
        author_did,
        author_handle: None,
        text: text_body,
        post_uri,
        record,
        cursor: payload.seq,
        at_us,
    }))
}

/// The veles consumer's gate: is this event a POST addressed to the bot?
/// Applied by the adapter on its subscription, leaving the broadcast
/// stream broad for other consumers (courier).
pub fn gates_bot(bot_did: &str, ev: &MatchedEvent) -> bool {
    ev.record.get("$type").and_then(|t| t.as_str()) == Some("app.bsky.feed.post")
        && crate::event::gate_bot_post(bot_did, &ev.author_did, &ev.record) == Gate::Addressed
}

/// Durable cursor persistence: write-temp-then-rename, so a crash cannot
/// leave a half-written cursor. Throttled by TIME: the cursor moves with
/// every network-wide commit, and "every 100 events" was several file
/// writes a second, all day. A restart replays at most `every` of
/// stream, which the gateway's dedup absorbs.
pub struct CursorStore {
    path: std::path::PathBuf,
    /// In-memory last-saved, so save() can no-op when nothing moved.
    last: std::sync::Mutex<Option<i64>>,
    /// Write at most once per this interval.
    every: std::time::Duration,
    /// When the cursor was last written (None = never this run).
    written: std::sync::Mutex<Option<std::time::Instant>>,
}

impl CursorStore {
    pub fn new(path: std::path::PathBuf, every: std::time::Duration) -> Self {
        Self {
            path,
            last: std::sync::Mutex::new(None),
            every,
            written: std::sync::Mutex::new(None),
        }
    }

    /// The persisted cursor, if any (a crash resumes from the live tip).
    pub fn load(&self) -> Result<Option<i64>> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => {
                let v = s.trim().parse::<i64>().ok();
                *self.last.lock().unwrap() = v;
                Ok(v)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("cursor load"),
        }
    }

    /// Persist the cursor, throttled. Cheap to call per event.
    pub fn save(&self, cursor: i64) {
        {
            let w = self.written.lock().unwrap();
            if w.is_some_and(|t| t.elapsed() < self.every) {
                return;
            }
        }
        self.save_now(cursor);
    }

    /// Drop the persisted cursor: the next connect is live. Used when a
    /// resumed connection delivered nothing (the server will not serve
    /// that cursor and does not say so).
    pub fn forget(&self) {
        let _ = std::fs::remove_file(&self.path);
        *self.last.lock().unwrap() = None;
        *self.written.lock().unwrap() = None;
    }

    /// Persist immediately (on clean shutdown).
    pub fn save_now(&self, cursor: i64) {
        if self
            .last
            .lock()
            .unwrap()
            .map(|l| l == cursor)
            .unwrap_or(false)
        {
            return;
        }
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, cursor.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
            *self.last.lock().unwrap() = Some(cursor);
            *self.written.lock().unwrap() = Some(std::time::Instant::now());
        }
    }
}

/// The processing backlog that counts as falling behind (the channel
/// holds 4096 frames).
const BACKLOG_BEHIND: usize = 2048;
/// At most one "falling behind" line per this, however long it lasts.
const BACKLOG_WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// What the backlog says, once per stretch instead of once per frame:
/// a reconnect replaying from the cursor fills the channel at the
/// relay's speed, and the per-frame warning wrote 201 lines in 2 ms
/// (VPS, 2026-10-10).
#[derive(Default)]
struct BacklogWatch {
    /// When the current stretch last warned; `None` = not behind.
    warned_at: Option<std::time::Instant>,
    peak: usize,
}

enum BacklogLine {
    Behind(String),
    CaughtUp(String),
}

impl BacklogWatch {
    fn observe(&mut self, backlog: usize, now: std::time::Instant) -> Option<BacklogLine> {
        if backlog > BACKLOG_BEHIND {
            self.peak = self.peak.max(backlog);
            let due = self
                .warned_at
                .is_none_or(|t| now.duration_since(t) >= BACKLOG_WARN_EVERY);
            if !due {
                return None;
            }
            self.warned_at = Some(now);
            return Some(BacklogLine::Behind(format!(
                "atproto jetstream: processing backlog {backlog} frames — falling behind"
            )));
        }
        self.warned_at?;
        let peak = std::mem::take(&mut self.peak);
        self.warned_at = None;
        Some(BacklogLine::CaughtUp(format!(
            "atproto jetstream: caught up (the backlog peaked at {peak} frames)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "did:plc:bot";

    fn jetstream_post(did: &str, mention: Option<&str>) -> String {
        let facets = match mention {
            Some(m) => serde_json::json!([{"index":{"byteStart":0,"byteEnd":3},
                "features":[{"$type":"app.bsky.richtext.facet#mention","did":m}]}]),
            None => serde_json::Value::Null,
        };
        v2_commit(
            did,
            "create",
            "app.bsky.feed.post",
            serde_json::json!({ "$type":"app.bsky.feed.post", "text":"hi @bot", "facets": facets }),
        )
    }

    /// A Jetstream v2 commit frame, in the shape captured from the live
    /// endpoint on 2026-10-06.
    fn v2_commit(
        did: &str,
        operation: &str,
        collection: &str,
        record: serde_json::Value,
    ) -> String {
        serde_json::json!({ "$type": "message", "payload": {
            "$type": "network.bsky.jetstream.subscribeEvents#commit",
            "did": did, "seq": 26695517704i64, "time": "2026-10-06T13:33:48.707461Z",
            "witnessedAt": "2026-10-06T13:33:48.707461Z",
            "operation": operation, "collection": collection, "rkey": "rk", "rev": "r",
            "cid": "bafy", "record": record } })
        .to_string()
    }

    #[test]
    fn a_post_mentioning_the_bot_parses_and_passes_the_consumer_gate() {
        let m = parse_event(&jetstream_post("did:plc:alice", Some(BOT))).unwrap();
        assert_eq!(m.author_did, "did:plc:alice");
        assert_eq!(m.text, "hi @bot");
        assert_eq!(m.post_uri, "at://did:plc:alice/app.bsky.feed.post/rk");
        assert_eq!(m.lane, Lane::Jetstream);
        assert_eq!(m.key, "at://did:plc:alice/app.bsky.feed.post/rk#bafy");
        // The consumer gate confirms it.
        assert!(gates_bot(BOT, &m));
    }

    #[test]
    fn the_bots_own_post_never_passes_its_gate() {
        // A self-mention or a thread continuation by the bot itself.
        let mine = parse_event(&jetstream_post(BOT, Some(BOT))).unwrap();
        assert!(!gates_bot(BOT, &mine));
    }

    #[test]
    fn the_stream_is_broad_but_the_consumer_gate_is_strict() {
        // A post NOT addressed to the bot still PARSES (it is broadcast for
        // other consumers like courier) but FAILS the veles bot gate.
        let other = parse_event(&jetstream_post("did:plc:alice", Some("did:plc:other"))).unwrap();
        assert!(!gates_bot(BOT, &other));
        let no_mention = parse_event(&jetstream_post("did:plc:alice", None)).unwrap();
        assert!(!gates_bot(BOT, &no_mention));
        // A like/repost/follow parses too (courier pushes for those) and is
        // never a bot-gate hit (not a post).
        let like = v2_commit(
            "did:plc:x",
            "create",
            "app.bsky.feed.like",
            serde_json::json!({"$type":"app.bsky.feed.like"}),
        );
        let like_ev = parse_event(&like).unwrap();
        assert!(!gates_bot(BOT, &like_ev));
    }

    #[test]
    fn non_commit_and_non_create_frames_are_dropped() {
        let account = serde_json::json!({"$type":"message","payload":{
            "$type":"network.bsky.jetstream.subscribeEvents#account",
            "did":"did:plc:x","seq":7,"account":{}}})
        .to_string();
        assert!(parse_event(&account).is_none());
        let delete = v2_commit(
            "did:plc:x",
            "delete",
            "app.bsky.feed.post",
            serde_json::Value::Null,
        );
        assert!(parse_event(&delete).is_none());
        // A collection outside the app-notif set is dropped.
        let other = v2_commit(
            "did:plc:x",
            "create",
            "app.bsky.actor.profile",
            serde_json::json!({"$type":"app.bsky.actor.profile"}),
        );
        assert!(parse_event(&other).is_none());
    }

    /// v2's cursor is the event's `seq`; its time is `time`. They used to
    /// be one number (v1's `time_us`), and the latency log read the cursor
    /// as a time.
    #[test]
    fn the_cursor_is_the_seq_and_the_time_is_the_commit_time() {
        let m = parse_event(&jetstream_post("did:plc:alice", Some(BOT))).unwrap();
        assert_eq!(m.cursor, Some(26695517704));
        assert_eq!(m.at_us, Some(1_791_293_628_707_461));
    }

    #[test]
    fn an_outdated_cursor_notice_is_an_info_frame() {
        let info = serde_json::json!({"$type":"message","payload":{
            "$type":"network.bsky.jetstream.subscribeEvents#info","name":"OutdatedCursor",
            "message":"requested timestamp cursor below retention floor; starting at seq 26650021043"}})
        .to_string();
        match parse_frame(&info) {
            Frame::Info { name, message } => {
                assert_eq!(name, "OutdatedCursor");
                assert!(message.contains("retention floor"));
            }
            other => panic!("not an info frame: {other:?}"),
        }
    }

    /// The v1 wire format parses to nothing: this lane reads v2 only.
    #[test]
    fn a_v1_frame_is_not_an_event() {
        let v1 = serde_json::json!({
            "did": "did:plc:alice", "time_us": 1728000000000000i64, "kind": "commit",
            "commit": { "operation":"create", "collection":"app.bsky.feed.post", "rkey":"rk",
                "record": { "$type":"app.bsky.feed.post", "text":"hi" } } })
        .to_string();
        assert!(parse_event(&v1).is_none());
    }

    #[test]
    fn the_url_names_v2_collections_and_the_cursor() {
        let cols = vec![
            "app.bsky.feed.post".to_string(),
            "app.bsky.feed.like".to_string(),
        ];
        assert_eq!(
            subscribe_url(
                "wss://js/xrpc/network.bsky.jetstream.subscribeEvents",
                &cols,
                Some(42)
            ),
            "wss://js/xrpc/network.bsky.jetstream.subscribeEvents\
             ?collections=app.bsky.feed.post&collections=app.bsky.feed.like&cursor=42"
        );
        assert_eq!(subscribe_url("wss://js/s", &[], None), "wss://js/s");
    }

    /// The server closing the socket is not a shutdown: the lane reconnects,
    /// resuming at the last `seq`. It used to return, ending the lane until
    /// the gateway restarted.
    /// The 2026-10-09 loop: a cursor the server completes the handshake
    /// for and then never serves. The first session (resumed) ends with
    /// nothing delivered, so the lane forgets the cursor and the second
    /// connect is LIVE (no `cursor=`); a cursor that DID deliver is kept.
    #[tokio::test]
    async fn a_resume_the_server_answers_with_silence_is_dropped() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (uri_tx, mut uri_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let post = jetstream_post("did:plc:alice", Some(BOT));
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                n += 1;
                let uri_tx = uri_tx.clone();
                #[allow(clippy::result_large_err)]
                let callback = move |req: &Request, resp: Response| {
                    uri_tx.send(req.uri().to_string()).unwrap();
                    Ok(resp)
                };
                let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback)
                    .await
                    .unwrap();
                if n == 1 {
                    // The resumed session: silence, then the server closes.
                    let _ = ws.close(None).await;
                } else {
                    ws.send(Message::Text(post.clone())).await.unwrap();
                    let _ = ws.close(None).await;
                }
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let cursor_path = dir.path().join("j.cursor");
        std::fs::write(&cursor_path, "26000000000").unwrap();
        let cursor = Arc::new(CursorStore::new(
            cursor_path.clone(),
            std::time::Duration::ZERO,
        ));
        let (filter, handle) = Filter::new(16);
        let mut events = filter.subscribe();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let url = format!("ws://{addr}/xrpc/network.bsky.jetstream.subscribeEvents");
        let cols = vec!["app.bsky.feed.post".to_string()];
        let lane =
            tokio::spawn(async move { run_jetstream(&url, &cols, handle, cursor, stop_rx).await });

        let first = uri_rx.recv().await.unwrap();
        assert!(first.ends_with("cursor=26000000000"), "resumed: {first}");
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), uri_rx.recv())
            .await
            .expect("no reconnect")
            .unwrap();
        assert!(
            !second.contains("cursor="),
            "the silent resume's cursor was kept: {second}"
        );
        assert!(!cursor_path.exists(), "the cursor file survived");
        // The live session delivered: its cursor is saved and resumed.
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.cursor, Some(26695517704));
        let third = tokio::time::timeout(std::time::Duration::from_secs(10), uri_rx.recv())
            .await
            .expect("no third connect")
            .unwrap();
        assert!(
            third.ends_with("cursor=26695517704"),
            "a delivering cursor is kept: {third}"
        );

        stop_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), lane)
            .await
            .expect("the lane ignored shutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn the_lane_reconnects_after_the_server_closes() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (uri_tx, mut uri_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let post = jetstream_post("did:plc:alice", Some(BOT));
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let uri_tx = uri_tx.clone();
                #[allow(clippy::result_large_err)] // tungstenite's callback type
                let callback = move |req: &Request, resp: Response| {
                    uri_tx.send(req.uri().to_string()).unwrap();
                    Ok(resp)
                };
                let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback)
                    .await
                    .unwrap();
                ws.send(Message::Text(post.clone())).await.unwrap();
                let _ = ws.close(None).await;
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let cursor = Arc::new(CursorStore::new(
            dir.path().join("j.cursor"),
            std::time::Duration::ZERO,
        ));
        let (filter, handle) = Filter::new(16);
        let mut events = filter.subscribe();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let url = format!("ws://{addr}/xrpc/network.bsky.jetstream.subscribeEvents");
        let cols = vec!["app.bsky.feed.post".to_string()];
        let lane =
            tokio::spawn(async move { run_jetstream(&url, &cols, handle, cursor, stop_rx).await });

        let first = uri_rx.recv().await.unwrap();
        assert!(first.contains("collections=app.bsky.feed.post"), "{first}");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.cursor, Some(26695517704));
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), uri_rx.recv())
            .await
            .expect("the lane did not reconnect after the server closed")
            .unwrap();
        assert!(second.ends_with("cursor=26695517704"), "{second}");

        stop_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), lane)
            .await
            .expect("the lane ignored shutdown")
            .unwrap()
            .unwrap();
    }

    #[test]
    fn malformed_json_is_dropped_not_a_crash() {
        assert!(parse_event("{not json").is_none());
        assert!(parse_event("").is_none());
    }

    #[test]
    fn cursor_store_round_trips_and_throttles_by_time() {
        let dir = tempfile::tempdir().unwrap();
        let cs = CursorStore::new(
            dir.path().join("j.cursor"),
            std::time::Duration::from_secs(3600),
        );
        assert_eq!(cs.load().unwrap(), None);
        cs.save(1); // the first save of a run writes
        assert_eq!(cs.load().unwrap(), Some(1));
        for c in 2..500 {
            cs.save(c); // all inside the interval: throttled away
        }
        assert_eq!(cs.load().unwrap(), Some(1), "no write within the interval");
        cs.save_now(10); // explicit flush
        assert_eq!(cs.load().unwrap(), Some(10));
        // A zero interval writes every time.
        let every = CursorStore::new(dir.path().join("k.cursor"), std::time::Duration::ZERO);
        every.save(5);
        every.save(6);
        assert_eq!(every.load().unwrap(), Some(6));
    }

    /// One "falling behind" line per stretch (and per 30 s of it), one
    /// "caught up" when it ends; the per-frame warning is gone.
    #[test]
    fn a_backlog_warns_once_per_stretch_and_says_when_it_caught_up() {
        let t0 = std::time::Instant::now();
        let mut w = BacklogWatch::default();
        assert!(w.observe(100, t0).is_none(), "not behind");
        assert!(matches!(w.observe(2100, t0), Some(BacklogLine::Behind(_))));
        for n in 0..200 {
            let at = t0 + std::time::Duration::from_millis(n);
            assert!(
                w.observe(2100 + n as usize, at).is_none(),
                "frame {n} warned again"
            );
        }
        let later = t0 + BACKLOG_WARN_EVERY;
        assert!(matches!(
            w.observe(3000, later),
            Some(BacklogLine::Behind(_))
        ));
        match w.observe(10, later) {
            Some(BacklogLine::CaughtUp(line)) => assert!(line.contains("3000"), "{line}"),
            _ => panic!("no caught-up line"),
        }
        assert!(w.observe(10, later).is_none(), "said twice");
        assert!(
            matches!(w.observe(2049, later), Some(BacklogLine::Behind(_))),
            "a new stretch"
        );
    }
}
