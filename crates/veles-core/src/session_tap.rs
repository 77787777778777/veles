//! Session event bus — one running turn, many watchers.
//!
//! This is `irc_tap` applied to chat sessions, deliberately and almost
//! line for line, because the IRC side of this codebase already solved
//! the problem and the chat side never got the fix.
//!
//! ## What was wrong
//!
//! A turn's events used to travel down a `tokio::sync::mpsc` channel
//! created inside the `POST /api/chat` handler, with the SSE response
//! body as its only reader (`desktop_api.rs`). mpsc is
//! SINGLE-CONSUMER, so:
//!
//!   - a second window could not watch a turn, ever;
//!   - a client that reconnected could not rejoin one either — there was
//!     no second receiver to hand it;
//!   - once the reader was gone the sends failed and were discarded, so
//!     a turn that outlived its window ran completely blind;
//!   - and `session_retry` had to refuse to start a turn at all,
//!     documenting the reason in its own error: "spawning a turn
//!     directly here would need to wire up the SSE event stream, which
//!     is session-scoped".
//!
//! Those are one bug wearing four coats, and the user reported all four
//! separately.
//!
//! ## The shape
//!
//! A process-wide `broadcast` channel plus, per live session, a
//! `TurnSnapshot` of the turn in flight. Late subscribers see only new
//! events — exactly as in `irc_tap`, where the doc reads "what came
//! before is the store's job". Here the store's job is real: `messages`
//! is keyed by `msg_id INTEGER PRIMARY KEY AUTOINCREMENT` with an index
//! on `(session_id, msg_id)`, and `engine_notes` anchors to
//! `after_msg_id`, so "everything since N" is one indexed query.
//!
//! The snapshot covers the gap the store cannot: the assistant message
//! being streamed RIGHT NOW, which is not persisted until it completes.
//! Without it a client attaching mid-turn sees the transcript up to the
//! last finished message and then nothing until the turn ends — which is
//! precisely the "session view is stuck on a very old message" report.
//!
//! ## Why events are built here and not in the HTTP layer
//!
//! `SessionEvent` is the WIRE shape, converted from `StreamEvent` once,
//! in `from_stream`. The desktop's SSE handler used to do that
//! conversion inline, so the bus and the endpoint could have drifted
//! into describing the same turn two ways. One conversion, one vocabulary.

use crate::providers::StreamEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::broadcast;

/// Same order of magnitude as `irc_tap`'s: a turn streaming tokens is
/// far chattier than an IRC connection, and a slow client must lag
/// (which is reported) rather than stall the turn.
const CHANNEL_CAPACITY: usize = 4096;

/// One thing that happened in a session.
///
/// `kind` is the SSE event name the desktop already switches on
/// (`delta`, `reasoning`, `route`, `tool`, `note`, `ask`), plus the two
/// lifecycle events the old design had no way to express at all
/// (`turn-started`, `turn-ended`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic within a process run, across ALL sessions. Orders live
    /// events for subscribers; the durable cursor is `msg_id` in the
    /// store, not this.
    pub seq: u64,
    pub session_id: String,
    pub kind: String,
    pub data: serde_json::Value,
}

/// One completed round of a turn: the reasoning and answer text that
/// led up to a tool call, and the call itself. The turn's LIVE tail
/// (everything since the last round) stays in `TurnSnapshot::reasoning`
/// and `::content`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotRound {
    pub reasoning: String,
    pub content: String,
    pub name: String,
    pub args: String,
    pub result: String,
}

/// The turn in flight, for a client that attaches in the middle of one.
///
/// Everything here is re-derivable from the events already broadcast —
/// it exists because a late subscriber did not receive them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnSnapshot {
    pub session_id: String,
    /// Which surface started it (desktop/telegram/irc/cron/…). A turn
    /// started from IRC is visible to the desktop through this — the
    /// old design could not see it at all, because liveness was read off
    /// whichever `Agent` instance the desktop happened to hold.
    pub surface: String,
    /// RFC3339, when the turn began.
    pub started_at: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// Assistant text streamed so far and NOT yet persisted.
    pub content: String,
    /// Reasoning streamed so far, same caveat.
    pub reasoning: String,
    /// Rounds completed so far this turn, in order, each carrying the
    /// thinking and text that PRECEDED its tool call.
    ///
    /// It used to be `tools: Vec<(name, args, result)>` with the
    /// reasoning thrown away: `ToolEnd` did `s.reasoning.clear()`, so a
    /// turn with N tool rounds handed a late subscriber round N's
    /// thinking and nothing of rounds 1..N-1. The desktop's mid-turn
    /// re-handshake read exactly this, which is why switching back to a
    /// running session showed "most of the newest thinking blocks
    /// missing" (USER 2026-09-16). A snapshot exists to tell a late
    /// subscriber what it missed; one that drops most of it is worse
    /// than none, because it looks complete.
    pub rounds: Vec<SnapshotRound>,
    /// The highest `msg_id` this session had when the turn started, so a
    /// client knows where the persisted transcript ends and this
    /// snapshot begins.
    pub base_msg_id: i64,
    /// Epoch ms of the last event that touched this snapshot.
    ///
    /// `turn_ended` is the ONLY thing that removes a snapshot, so a
    /// turn that never reaches it — a panic, a killed child, a gateway
    /// that lost the process writing the session — leaves one standing
    /// for the life of the process. Anything that reads "a snapshot
    /// exists" as "a live turn owns this transcript" then waits on a
    /// corpse, and the attached-stream follow tick is exactly such a
    /// reader. `started_at` cannot answer this: a turn can legitimately
    /// run for many minutes. Only "when did something last happen" can,
    /// so every event restamps it. See `snapshot_fresh`.
    #[serde(default)]
    pub last_event_at: u64,
    /// The process running the turn, when it is NOT this one: a
    /// delegate child whose events reach this gateway through the relay
    /// (`relay_ingest`). This process only mirrors such a turn, so the
    /// child's death has to end it here — a child killed at its timeout
    /// never sends `turn-ended`, and the mirror heartbeated the session
    /// as "running" for 18 hours (2026-10-04, three translation
    /// subagents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
}

/// Epoch ms, the clock `last_event_at` is kept in.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Bus {
    tx: broadcast::Sender<SessionEvent>,
    seq: AtomicU64,
    /// session id -> the turn currently running in it
    live: Mutex<HashMap<String, TurnSnapshot>>,
    /// DESKTOP turns that ended while no notify stream was listening, for
    /// the phone notifier's reconnect (`notify_listen`).
    missed: Mutex<Missed>,
}

/// The notify streams listening now, and the desktop turns that ended
/// while there were none. One lock for both, so a turn's end and a
/// stream's arrival cannot cross: a turn is either seen live by a stream
/// or kept for the next one, never both and never neither.
#[derive(Default)]
struct Missed {
    listeners: usize,
    turns: std::collections::VecDeque<String>,
}

/// How many missed desktop turns the notify replay keeps. A phone that
/// was disconnected for hours does not want a hundred stale "turn done"
/// banners; the newest handful is the useful ones.
const MISSED_CAP: usize = 8;

static BUS: OnceLock<Bus> = OnceLock::new();

fn bus() -> &'static Bus {
    BUS.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Bus {
            tx,
            seq: AtomicU64::new(0),
            live: Mutex::new(HashMap::new()),
            missed: Mutex::new(Missed::default()),
        }
    })
}

/// The store handle for the CROSS-PROCESS liveness registry — same
/// publish-once pattern as `hooks::set_default_bus`, published from
/// `Agent::new`. The bus above answers "is a turn running in THIS
/// process"; `session_runs` answers "is a turn running at all", which
/// is what a desktop looking at a gateway's session, or a TUI looking
/// at a cron-fired one, needs. Writes are best-effort: a missing store
/// (tests, ephemeral agents) simply keeps liveness process-local.
static RUNS_STORE: OnceLock<Mutex<Option<Arc<Mutex<crate::store::Store>>>>> = OnceLock::new();

pub fn set_runs_store(store: Arc<Mutex<crate::store::Store>>) {
    let cell = RUNS_STORE.get_or_init(|| Mutex::new(None));
    if let Ok(mut g) = cell.lock() {
        *g = Some(store);
    }
}

fn runs_store() -> Option<Arc<Mutex<crate::store::Store>>> {
    RUNS_STORE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|g| g.clone())
}

/// The box's hostname, for the `host` column — a run row is only
/// meaningful when you can tell WHICH machine's pid it names. Linux
/// keeps it at a fixed path; anything else gets an honest "unknown"
/// rather than an invented answer.
pub(crate) fn hostname() -> String {
    crate::platform::hostname()
}

/// Is `pid` a process on this host? The platform layer's probe (signal
/// 0, or an open process handle): "gone" ends a turn whose process
/// died, on every platform — the old non-unix "always alive" kept a
/// dead subagent's turn running forever.
fn pid_alive(pid: u32) -> bool {
    crate::platform::pid_alive(pid)
}

/// How often a live turn is heartbeated (and a mirrored one's owner
/// looked for).
fn beat_every() -> std::time::Duration {
    if cfg!(test) {
        std::time::Duration::from_millis(50)
    } else {
        std::time::Duration::from_secs(5)
    }
}

/// Heartbeat a live turn every few seconds, from a thread that dies
/// with the turn: it exits as soon as THIS turn leaves the live map
/// (`started_at` tells it from a later turn in the same session), so a
/// process cannot outlive its own heartbeats — and a process that DIES
/// leaves stale rows for `runs_live`'s reaper, which is the other half
/// of the same guarantee.
///
/// A mirrored turn (`owner`, a relayed child on this host) is the case
/// that guarantee does not cover: the heartbeats come from HERE, so the
/// owner dying stops nothing. The thread watches the owner's pid and
/// ends the turn once it is gone.
fn spawn_heartbeat(session_id: String, started_at: String, owner: Option<u32>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(beat_every());
        let still = bus().live.lock().ok().is_some_and(|l| {
            l.get(&session_id)
                .is_some_and(|s| s.started_at == started_at)
        });
        if !still {
            break;
        }
        if owner.is_some_and(|pid| !pid_alive(pid)) {
            tracing::info!(
                "session {session_id}: the process running its turn (pid {}) is gone; \
                 ending the turn it never ended",
                owner.unwrap_or_default()
            );
            end_turn_if(&session_id, &started_at);
            break;
        }
        if let Some(s) = runs_store() {
            if let Ok(st) = s.lock() {
                let _ = st.run_heartbeat(&session_id);
            }
        }
    });
}

/// Watch every session, from now on.
///
/// Subscribers filter by `session_id`, the way `irc_tap`'s filter by
/// buffer. One channel rather than one per session: a session is created
/// and destroyed constantly, and a per-session channel would have to be
/// reaped, which is a lifetime problem this does not need.
pub fn subscribe() -> broadcast::Receiver<SessionEvent> {
    bus().tx.subscribe()
}

/// Publish one event, stamping it with the next sequence number.
pub fn publish(session_id: &str, kind: &str, data: serde_json::Value) -> u64 {
    let b = bus();
    let seq = b.seq.fetch_add(1, Ordering::SeqCst) + 1;
    // Freshness is stamped HERE and not in `with_snapshot`, because
    // three arms of `publish_stream` — Note, Ask, Cancelled — never
    // touch the snapshot at all. An `Ask` is the one that matters: a
    // turn blocked on a question can sit for minutes and is entirely
    // alive, and stamping only on mutation would let its snapshot rot
    // and hand its transcript to the follow tick mid-question.
    // Every event, no exceptions. (No caller holds `live` across a
    // publish — checked at all five lock sites.)
    if let Ok(mut live) = b.live.lock() {
        if let Some(s) = live.get_mut(session_id) {
            s.last_event_at = now_ms();
        }
    }
    // A send with no subscribers is not an error — the gateway runs with
    // nothing attached most of the time. This is the ONE place that is
    // true; the old per-request channel discarded events the same way
    // and it was a bug there, because that channel was the only copy.
    let _ = b.tx.send(SessionEvent {
        seq,
        session_id: session_id.to_string(),
        kind: kind.to_string(),
        data,
    });
    seq
}

pub fn latest_seq() -> u64 {
    bus().seq.load(Ordering::SeqCst)
}

/// The largest text a merged frame may carry before it is sent as it
/// stands. A burst can deliver a quarter of a megabyte at once (measured:
/// `examples/stream_pacing.rs` sees 212KB inside one 239ms window), and
/// while one big frame beats 766 small ones, an unbounded one is just a
/// different way to make a single read expensive.
const MERGE_CAP: usize = 64 * 1024;

/// Fold `next` into `head` when the two are adjacent text deltas of the
/// same kind in the same session, answering whether it happened.
///
/// This is the whole of the coalescing policy, kept as a pure function so
/// it can be reasoned about and tested without a socket. Only `delta` and
/// `reasoning` merge: they are the two kinds whose payload is a text
/// fragment that the client concatenates anyway, so joining two of them
/// changes the framing and not one character of the result. Everything
/// else — `tool`, `note`, `route`, `ask`, the lifecycle events — carries
/// structure that must arrive as its own event and in its own place in
/// the order, so a run of deltas STOPS at the first one.
pub fn merge_delta(head: &mut SessionEvent, next: &SessionEvent) -> bool {
    if head.session_id != next.session_id || head.kind != next.kind {
        return false;
    }
    if head.kind != "delta" && head.kind != "reasoning" {
        return false;
    }
    let Some(add) = next.data.get("text").and_then(|t| t.as_str()) else {
        return false;
    };
    let Some(have) = head.data.get("text").and_then(|t| t.as_str()) else {
        return false;
    };
    if have.len() + add.len() > MERGE_CAP {
        return false;
    }
    let joined = format!("{have}{add}");
    head.data["text"] = serde_json::Value::String(joined);
    // The sequence number becomes the LAST one folded in, so a client
    // that tracks seq sees the stream advance to where it really is
    // rather than to the first event of a merged run.
    head.seq = next.seq;
    true
}
/// Register a turn as live and broadcast `turn-started`.
///
/// `prompt` is the user message that kicked off the turn. A window that is
/// ALREADY attached (a second desktop, say) did not paint it optimistically
/// — the window that sent it did. `base_msg_id` is the persisted boundary
/// before provider output; `prompt_msg_id` identifies the user row itself
/// at or below that boundary. They are not interchangeable.
pub fn turn_started(session_id: &str, surface: &str, base_msg_id: i64, prompt: &str) {
    turn_started_with_prompt(session_id, surface, base_msg_id, 0, prompt, None, None);
}

/// The production turn announcement, including the actual persisted user
/// row id for clients joining optimistic paint to history replay.
pub fn turn_started_with_prompt(
    session_id: &str,
    surface: &str,
    base_msg_id: i64,
    prompt_msg_id: i64,
    prompt: &str,
    owner: Option<(u32, String)>,
    injection: Option<&str>,
) {
    turn_started_full(
        session_id,
        surface,
        base_msg_id,
        prompt_msg_id,
        prompt,
        owner,
        injection,
    );
}

/// `turn_started` for a turn another process runs and this one mirrors
/// (`owner` = that process's pid and host, from its relayed
/// `turn-started`). The registry row names the owner, not this process,
/// and when the owner is on this host its death ends the turn here.
pub fn turn_started_by(
    session_id: &str,
    surface: &str,
    base_msg_id: i64,
    prompt: &str,
    owner: Option<(u32, String)>,
) {
    turn_started_full(session_id, surface, base_msg_id, 0, prompt, owner, None);
}

/// `turn_started` for a turn whose prompt row carries an injection tag —
/// a mail turn's `mail:<sender>` (`crate::mail`). A window that did not
/// start the turn paints the announced prompt itself, and it must paint
/// another session's words as mail, not as something its human typed.
pub fn turn_started_tagged(
    session_id: &str,
    surface: &str,
    base_msg_id: i64,
    prompt: &str,
    injection: Option<&str>,
) {
    turn_started_full(session_id, surface, base_msg_id, 0, prompt, None, injection);
}

/// The whole of it: an owner (a mirrored turn) and an injection tag.
pub fn turn_started_full(
    session_id: &str,
    surface: &str,
    base_msg_id: i64,
    prompt_msg_id: i64,
    prompt: &str,
    owner: Option<(u32, String)>,
    injection: Option<&str>,
) {
    let me = hostname();
    let (pid, host) = owner
        .clone()
        .unwrap_or_else(|| (std::process::id(), me.clone()));
    // A pid only means something on its own host: a turn relayed from
    // elsewhere keeps today's rule (it lives until its `turn-ended`).
    let watch = owner.filter(|(_, h)| *h == me).map(|(p, _)| p);
    let snap = TurnSnapshot {
        session_id: session_id.to_string(),
        surface: surface.to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        base_msg_id,
        last_event_at: now_ms(),
        owner_pid: watch,
        ..Default::default()
    };
    if let Ok(mut live) = bus().live.lock() {
        live.insert(session_id.to_string(), snap.clone());
    }
    // The store registry: one row, whichever process is running this
    // turn, heartbeated by the thread below and reaped by readers when
    // it goes silent. Best-effort by design — the bus above remains
    // the in-process source of truth.
    let store = runs_store();
    if let Some(s) = &store {
        if let Ok(st) = s.lock() {
            let _ = st.run_start(session_id, pid, &host, surface);
        }
    }
    if store.is_some() || watch.is_some() {
        spawn_heartbeat(session_id.to_string(), snap.started_at.clone(), watch);
    }
    publish(
        session_id,
        "turn-started",
        json!({
            "surface": surface,
            "started_at": snap.started_at,
            "base_msg_id": base_msg_id,
            "prompt_msg_id": prompt_msg_id,
            "prompt": prompt,
            // The prompt row's injection tag, when it has one (mail).
            "injection": injection,
            // Who runs it, so a gateway mirroring this turn through the
            // relay can tell when it died (`relay_ingest`).
            "pid": pid,
            "host": host,
        }),
    );
}

/// Deregister a turn and broadcast `turn-ended`.
///
/// Called from `TurnGuard::drop`, so an early return, a `?` or a panic
/// cannot leave a session looking busy forever — the same reason
/// `LIVE_TURNS` is cleared there.
///
/// It carries NO verdict, deliberately. The guard fires on success, on
/// error and on panic alike and cannot tell them apart, so any `ok` flag
/// here would be a guess dressed as a fact. The division of labour is
/// the one the store already enforces: the bus says a turn STOPPED, the
/// persisted transcript says what it did. A client reloads from
/// `msg_id` either way, so there is nothing an outcome field would save
/// it and one more thing that could lie to it.
///
/// It does carry the SURFACE that started the turn, a fact rather than a
/// verdict: a listener that joined mid-turn saw no `turn-started` to
/// learn it from, and the phone's notifier tells a turn you started at
/// the desktop from one a cron job or a chat started (GET
/// /api/notify/events).
pub fn turn_ended(session_id: &str) {
    let surface = bus()
        .live
        .lock()
        .ok()
        .and_then(|mut live| live.remove(session_id))
        .map(|s| s.surface)
        .unwrap_or_default();
    // Deregister from the store too — the heartbeat thread notices the
    // live map on its next tick, but the row should not outlive the
    // turn by even that long: a reader between here and the tick would
    // call a finished turn "running".
    if let Some(s) = runs_store() {
        if let Ok(st) = s.lock() {
            let _ = st.run_end(session_id);
        }
    }
    // The phone notifier's replay: a DESKTOP (or a phone trigger's) turn that ended while no
    // notify stream listened is kept for the next one. A turn a stream
    // saw live is not — it was announced, or the desktop was in front
    // and showed it — so a reconnect never re-posts it.
    if surface == "desktop" || surface == "trigger" {
        if let Ok(mut m) = bus().missed.lock() {
            if m.listeners == 0 {
                m.turns.retain(|sid| sid != session_id);
                m.turns.push_back(session_id.to_string());
                while m.turns.len() > MISSED_CAP {
                    m.turns.pop_front();
                }
            }
        }
    }
    publish(session_id, "turn-ended", json!({ "surface": surface }));
}

/// A notify stream listening (GET /api/notify/events). While one is held
/// a desktop turn's end goes to it live and is not kept for a replay;
/// dropping it (the stream closed) starts keeping them again.
pub struct NotifyListener(());

impl Drop for NotifyListener {
    fn drop(&mut self) {
        if let Ok(mut m) = bus().missed.lock() {
            m.listeners = m.listeners.saturating_sub(1);
        }
    }
}

/// Start listening for the notifier: the desktop turns that ended while
/// nobody listened (oldest first, each handed over once), and the guard
/// that keeps further ends live-only. The stream subscribes to the bus
/// BEFORE calling this, so a turn that ends in between arrives twice
/// (kept and live) rather than never; the notifier posts both under one
/// notification id.
pub fn notify_listen() -> (NotifyListener, Vec<String>) {
    let mut m = match bus().missed.lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    m.listeners += 1;
    let turns = m.turns.drain(..).collect();
    (NotifyListener(()), turns)
}

/// Forget the missed turns. Process-global, so a test asserting an exact
/// notify frame sequence starts by emptying it — a parallel test's
/// finished desktop turn must not leak into its replay. Production code
/// never calls this.
#[doc(hidden)]
pub fn clear_missed_turns() {
    if let Ok(mut m) = bus().missed.lock() {
        m.turns.clear();
    }
}

/// `turn_ended`, but only for the turn that started at `started_at` —
/// a watcher that outlived its turn must not end the next one.
fn end_turn_if(session_id: &str, started_at: &str) {
    let ended = bus().live.lock().ok().and_then(|mut live| {
        let ours = live
            .get(session_id)
            .is_some_and(|s| s.started_at == started_at);
        if ours {
            live.remove(session_id)
        } else {
            None
        }
    });
    let Some(snap) = ended else {
        return;
    };
    if let Some(s) = runs_store() {
        if let Ok(st) = s.lock() {
            let _ = st.run_end(session_id);
        }
    }
    publish(
        session_id,
        "turn-ended",
        json!({ "surface": snap.surface, "owner_gone": true }),
    );
}

/// The turn in flight for this session, if any.
pub fn snapshot(session_id: &str) -> Option<TurnSnapshot> {
    bus().live.lock().ok()?.get(session_id).cloned()
}

/// The turn in flight for this session, if one has been heard from
/// within `max_age`.
///
/// Ask this, not `snapshot`, whenever the answer decides whether some
/// OTHER mechanism should stay out of the way — because "a snapshot
/// exists" and "a turn is alive" stopped being the same statement the
/// first time a turn died without reaching `turn_ended`. `snapshot`
/// itself stays as-is: a late subscriber attaching mid-turn wants
/// whatever there is, stale or not, since the alternative is showing
/// nothing.
pub fn snapshot_fresh(session_id: &str, max_age: std::time::Duration) -> Option<TurnSnapshot> {
    let s = snapshot(session_id)?;
    let age = now_ms().saturating_sub(s.last_event_at);
    (age < max_age.as_millis() as u64).then_some(s)
}

/// Broadcast `history-changed`: the persisted transcript changed OUTSIDE
/// a turn — compaction finished, `/clear`, `/undo`, `/branch` — so
/// clients must reload from the store rather than infer completion from
/// anything else.
///
/// The thing they used to infer it from was `/context`'s `busy` key, and
/// the inference was unsound: a compaction holds the AGENT lock but is
/// not a turn, so the endpoint answers `busy: false` from the summarizer's
/// first moment — absence of the key cannot distinguish "still
/// compacting" from "done". The event is the truth and it is published
/// by the operation itself, so no reader needs to guess. `last_msg_id`
/// is the store head AFTER the change (the client takes it as its resume
/// cursor before reloading).
pub fn history_changed(session_id: &str, reason: &str, last_msg_id: i64) {
    publish(
        session_id,
        "history-changed",
        json!({ "reason": reason, "last_msg_id": last_msg_id }),
    );
}

/// Every session with a turn in flight, whichever surface started it.
pub fn live_sessions() -> Vec<String> {
    match bus().live.lock() {
        Ok(l) => l.keys().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

fn with_snapshot<F: FnOnce(&mut TurnSnapshot)>(session_id: &str, f: F) {
    if let Ok(mut live) = bus().live.lock() {
        if let Some(s) = live.get_mut(session_id) {
            f(s);
        }
    }
}

/// Convert one `StreamEvent` to its wire form, update the snapshot, and
/// broadcast it.
///
/// `Done` is deliberately NOT forwarded. It means "this provider stream
/// finished", not "the turn did" — a turn with tool rounds emits several
/// — and every consumer that treated it as the end truncated multi-round
/// turns. The end of a turn is `turn-ended`, which is emitted exactly
/// once, from the guard.
pub fn publish_stream(session_id: &str, ev: &StreamEvent) {
    match ev {
        StreamEvent::Delta(t) => {
            with_snapshot(session_id, |s| s.content.push_str(t));
            publish(session_id, "delta", json!({ "text": t }));
        }
        StreamEvent::ReasoningDelta(t) => {
            with_snapshot(session_id, |s| s.reasoning.push_str(t));
            publish(session_id, "reasoning", json!({ "text": t }));
        }
        StreamEvent::Route {
            provider,
            model,
            reasoning_effort,
        } => {
            with_snapshot(session_id, |s| {
                s.provider = provider.clone();
                s.model = model.clone();
                s.reasoning_effort = reasoning_effort.clone();
            });
            publish(
                session_id,
                "route",
                json!({
                    "provider": provider,
                    "model": model,
                    "reasoning_effort": reasoning_effort,
                }),
            );
        }
        StreamEvent::ToolStart(name, args) => {
            with_snapshot(session_id, |s| {
                // The round TAKES the reasoning and text that led to it.
                // That is where they belong — the engine persists one
                // assistant row per provider call, and this call's row
                // carries exactly the thinking and text that preceded
                // its tool_use — and it is what makes a late subscriber
                // able to rebuild the turn's real shape instead of one
                // lump. `ToolEnd` used to `clear()` them into nothing
                // here instead, which is the bug.
                s.rounds.push(SnapshotRound {
                    reasoning: std::mem::take(&mut s.reasoning),
                    content: std::mem::take(&mut s.content),
                    name: name.clone(),
                    args: args.clone(),
                    result: String::new(),
                });
            });
            publish(
                session_id,
                "tool",
                json!({ "state": "start", "name": name, "args": args }),
            );
        }
        StreamEvent::ToolEnd(name, result) => {
            with_snapshot(session_id, |s| {
                // Close the most recent round of this name that is still
                // open. Matching by name rather than by position because
                // nothing guarantees a tool's end follows its own start
                // with no other round between.
                if let Some(t) = s
                    .rounds
                    .iter_mut()
                    .rev()
                    .find(|t| t.name == *name && t.result.is_empty())
                {
                    t.result = result.clone();
                }
                // NOTHING IS CLEARED HERE. `s.reasoning`/`s.content`
                // were taken by this round's ToolStart and hold only
                // what has streamed since — which belongs to the NEXT
                // assistant message, exactly as the old comment said.
                // Clearing them again was harmless; clearing them
                // INSTEAD of keeping them is what lost the turn.
            });
            publish(
                session_id,
                "tool",
                json!({ "state": "end", "name": name, "result": result }),
            );
        }
        StreamEvent::Note(n) => {
            publish(
                session_id,
                "note",
                serde_json::to_value(n).unwrap_or_else(|_| json!({})),
            );
        }
        StreamEvent::Ask(req) => {
            let mut data = serde_json::to_value(&**req).unwrap_or_else(|_| json!({}));
            // Which surface's turn is asking, stamped while the turn is
            // certainly running: a listener that decides by it (the
            // phone's notifier leaves IRC's and Telegram's asks to them)
            // may read the event after the turn is gone.
            if let Some(s) = snapshot(session_id) {
                data["surface"] = json!(s.surface);
            }
            publish(session_id, "ask", data);
        }
        StreamEvent::Cancelled => {
            publish(session_id, "cancelled", json!({}));
        }
        StreamEvent::Done(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a wire event by hand — the merge policy is about framing,
    /// so the tests are about framing and need no bus.
    fn ev(sid: &str, kind: &str, text: &str, seq: u64) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: sid.into(),
            kind: kind.into(),
            data: json!({ "text": text }),
        }
    }

    #[test]
    fn two_adjacent_deltas_become_one_frame_carrying_both_in_order() {
        let mut head = ev("s", "delta", "the ", 1);
        assert!(merge_delta(&mut head, &ev("s", "delta", "answer", 2)));
        assert_eq!(head.data["text"], "the answer");
        // The seq advances to where the stream really is, not to where
        // the merged run started.
        assert_eq!(head.seq, 2);
    }

    #[test]
    fn reasoning_and_answer_text_never_fold_into_each_other() {
        let mut head = ev("s", "reasoning", "thinking", 1);
        assert!(!merge_delta(&mut head, &ev("s", "delta", "answer", 2)));
        assert_eq!(head.data["text"], "thinking");
    }

    /// The one that matters for ORDER: a tool round between two deltas
    /// must stay between them. Folding across it would move the ⚙ line
    /// after text it actually preceded, and the transcript's whole claim
    /// is that live reads like reload (R3).
    ///
    /// The fixture carries a `text` key ON PURPOSE. A real tool event has
    /// none, so it is refused by the payload shape whatever the kind
    /// check does — which would make this test green even with the kind
    /// guard deleted, i.e. a test that cannot fail for the reason it
    /// names. Giving it text puts the KIND guard under the assertion.
    #[test]
    fn a_tool_round_between_two_deltas_is_never_merged_across() {
        let mut head = ev("s", "delta", "before", 1);
        let tool = SessionEvent {
            seq: 2,
            session_id: "s".into(),
            kind: "tool".into(),
            data: json!({ "state": "start", "name": "read_file", "text": "after" }),
        };
        assert!(!merge_delta(&mut head, &tool));
        assert_eq!(head.data["text"], "before");
    }

    #[test]
    fn two_sessions_streaming_at_once_never_share_a_frame() {
        let mut head = ev("s1", "delta", "mine", 1);
        assert!(!merge_delta(&mut head, &ev("s2", "delta", "yours", 2)));
        assert_eq!(head.data["text"], "mine");
    }

    /// Structured events carry more than a text field; merging one would
    /// silently drop everything but `text`.
    #[test]
    fn a_note_is_never_folded_into_anything() {
        let mut head = SessionEvent {
            seq: 1,
            session_id: "s".into(),
            kind: "note".into(),
            data: json!({ "text": "a", "kind": "compaction" }),
        };
        let other = SessionEvent {
            seq: 2,
            session_id: "s".into(),
            kind: "note".into(),
            data: json!({ "text": "b", "kind": "compaction" }),
        };
        assert!(!merge_delta(&mut head, &other));
        assert_eq!(head.data["kind"], "compaction");
    }

    /// One big frame beats 766 small ones; an unbounded one is just a
    /// different way to make a single read expensive.
    #[test]
    fn a_merged_frame_stops_growing_at_the_cap() {
        let mut head = ev("s", "delta", &"x".repeat(MERGE_CAP - 4), 1);
        assert!(merge_delta(&mut head, &ev("s", "delta", "abcd", 2)));
        assert_eq!(head.data["text"].as_str().unwrap().len(), MERGE_CAP);
        // The next one does NOT fit, and is refused rather than truncated
        // — the caller holds it back for the following frame, so no text
        // is ever lost to the cap.
        assert!(!merge_delta(&mut head, &ev("s", "delta", "e", 3)));
        assert_eq!(head.data["text"].as_str().unwrap().len(), MERGE_CAP);
    }

    /// The property the whole change rests on: merging alters FRAMING and
    /// not one character of the stream. A burst folded on the way out
    /// must reassemble to exactly what arriving one-at-a-time produces.
    #[test]
    fn a_folded_burst_reassembles_to_the_same_text() {
        let chunks: Vec<String> = (0..500).map(|i| format!("tok{i} ")).collect();
        let whole: String = chunks.concat();
        let mut frames: Vec<SessionEvent> = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            let next = ev("s", "delta", c, i as u64);
            let folded = match frames.last_mut() {
                Some(head) => merge_delta(head, &next),
                None => false,
            };
            if !folded {
                frames.push(next);
            }
        }
        let rebuilt: String = frames
            .iter()
            .map(|f| f.data["text"].as_str().unwrap())
            .collect();
        assert_eq!(rebuilt, whole);
        assert!(
            frames.len() < chunks.len(),
            "500 adjacent deltas should not still be 500 frames"
        );
    }

    /// Take the next event for THIS session, skipping other sessions'.
    ///
    /// The bus is process-global — one channel for every session, the
    /// same shape as `irc_tap` — so under cargo's parallel test threads a
    /// subscriber sees other tests' traffic too. Real consumers filter by
    /// session id for exactly this reason; a test that did not would be
    /// testing a bus nobody uses.
    async fn recv_for(rx: &mut broadcast::Receiver<SessionEvent>, sid: &str) -> SessionEvent {
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("no event for this session within 5s")
                .expect("bus closed");
            if ev.session_id == sid {
                return ev;
            }
        }
    }

    /// Two subscribers both see a turn. This is the whole point: the old
    /// per-request mpsc could serve exactly one, so a second window and a
    /// reconnecting client were both impossible by construction.
    #[tokio::test]
    async fn two_subscribers_both_see_the_same_turn() {
        let sid = "two-watchers";
        let mut a = subscribe();
        let mut b = subscribe();
        publish_stream(sid, &StreamEvent::Delta("hello".into()));

        let ea = recv_for(&mut a, sid).await;
        let eb = recv_for(&mut b, sid).await;
        assert_eq!(ea.kind, "delta");
        assert_eq!(eb.kind, "delta");
        assert_eq!(ea.data["text"], "hello");
        assert_eq!(eb.data["text"], "hello");
        assert_eq!(ea.seq, eb.seq, "both see the SAME event, not copies");
    }

    /// A client that attaches mid-turn gets the text streamed before it
    /// arrived. Without this it sees the persisted transcript and then
    /// silence until the turn ends — the "stuck on a very old message"
    /// report.
    #[tokio::test]
    async fn a_late_subscriber_can_recover_the_partial_message() {
        let sid = "late-joiner";
        turn_started(sid, "desktop", 41, "");
        publish_stream(sid, &StreamEvent::Delta("the answer ".into()));
        publish_stream(sid, &StreamEvent::Delta("is 42".into()));

        // Attaching now: no events of its own, but the snapshot has it.
        let snap = snapshot(sid).expect("a live turn has a snapshot");
        assert_eq!(snap.content, "the answer is 42");
        assert_eq!(snap.base_msg_id, 41);
        assert_eq!(snap.surface, "desktop");

        turn_ended(sid);
        assert!(snapshot(sid).is_none(), "the turn is over");
    }

    /// A turn started anywhere is visible everywhere. The desktop used to
    /// read liveness off its OWN cached Agent, so a turn started by IRC,
    /// telegram or cron did not appear busy to it at all.
    #[tokio::test]
    async fn a_turn_started_by_another_surface_is_visible() {
        let sid = "irc-started";
        let mut rx = subscribe();
        turn_started(sid, "irc", 0, "do the thing");
        assert!(live_sessions().iter().any(|s| s == sid));
        assert_eq!(snapshot(sid).map(|s| s.surface), Some("irc".to_string()));
        // The prompt rides turn-started so an ALREADY-ATTACHED window — one
        // that did not paint it optimistically, because another surface sent
        // it — still shows the question above the reply about to stream in.
        let ev = recv_for(&mut rx, sid).await;
        assert_eq!(ev.kind, "turn-started");
        assert_eq!(ev.data["prompt"], "do the thing");
        turn_ended(sid);
        // The end names the surface too, for a listener that joined after
        // the start (the phone's notifier: a turn started at the desktop
        // is announced when it ends, one IRC started is not).
        let ev = recv_for(&mut rx, sid).await;
        assert_eq!(ev.kind, "turn-ended");
        assert_eq!(ev.data["surface"], "irc");
    }

    /// Tool rounds land in the snapshot paired with their results, and
    /// the text that preceded a round rides WITH that round rather than
    /// with the live tail — the message that made the call is where it
    /// belongs.
    #[tokio::test]
    async fn tool_rounds_pair_up_and_keep_the_text_that_led_to_them() {
        let sid = "tools";
        turn_started(sid, "desktop", 0, "");
        publish_stream(sid, &StreamEvent::Delta("thinking".into()));
        publish_stream(
            sid,
            &StreamEvent::ToolStart("read_file".into(), "{}".into()),
        );
        publish_stream(sid, &StreamEvent::ToolEnd("read_file".into(), "ok".into()));
        publish_stream(sid, &StreamEvent::Delta("done".into()));

        let snap = snapshot(sid).expect("live");
        assert_eq!(snap.rounds.len(), 1);
        assert_eq!(snap.rounds[0].name, "read_file");
        assert_eq!(snap.rounds[0].args, "{}");
        assert_eq!(snap.rounds[0].result, "ok");
        assert_eq!(
            snap.rounds[0].content, "thinking",
            "the text before the call belongs to the call's own message"
        );
        assert_eq!(snap.content, "done", "the live tail is what came after");
        turn_ended(sid);
    }

    /// THE SNAPSHOT MUST NOT LOSE THE EARLIER ROUNDS.
    ///
    /// `ToolEnd` used to `s.reasoning.clear()`, so a turn with N tool
    /// rounds could only ever hand a late subscriber round N's
    /// thinking. The desktop's mid-turn re-handshake read exactly this,
    /// which is why switching back to a running session showed "most of
    /// the newest thinking blocks are missing" (USER 2026-09-16) — the
    /// snapshot was not incomplete by accident, it was designed to
    /// throw the turn away one round at a time.
    #[tokio::test]
    async fn every_round_of_a_multi_round_turn_survives_in_the_snapshot() {
        let sid = "many-rounds";
        turn_started(sid, "desktop", 0, "");
        for (n, tool) in [("one", "read_file"), ("two", "terminal")] {
            publish_stream(sid, &StreamEvent::ReasoningDelta(format!("round {n}")));
            publish_stream(sid, &StreamEvent::ToolStart(tool.into(), "{}".into()));
            publish_stream(sid, &StreamEvent::ToolEnd(tool.into(), "ok".into()));
        }
        publish_stream(sid, &StreamEvent::ReasoningDelta("round three".into()));

        let snap = snapshot(sid).expect("live");
        assert_eq!(snap.rounds.len(), 2, "both rounds are kept");
        assert_eq!(
            snap.rounds[0].reasoning, "round one",
            "the FIRST round's thinking is the one the old code dropped"
        );
        assert_eq!(snap.rounds[1].reasoning, "round two");
        assert_eq!(
            snap.reasoning, "round three",
            "and the tail since the last round is still the tail"
        );
        turn_ended(sid);
    }

    /// `Done` fires once per provider round, not once per turn. Treating
    /// it as the end truncated every turn that used a tool, so it is not
    /// on the wire at all — `turn-ended` is, exactly once.
    #[tokio::test]
    async fn done_is_not_broadcast_but_turn_ended_is() {
        let sid = "no-done";
        let mut rx = subscribe();
        publish_stream(
            sid,
            &StreamEvent::Done(crate::providers::ChatMessage::assistant("x", None)),
        );
        turn_ended(sid);

        let ev = recv_for(&mut rx, sid).await;
        assert_eq!(ev.kind, "turn-ended", "Done must not reach the wire");
    }

    /// The cross-process registry (R4.5): turn_started/ended keep the
    /// store row in step with the live map, so `runs_live` — read by a
    /// DIFFERENT process, like the desktop looking at a gateway turn —
    /// answers the truth. This test owns the global runs-store: it is
    /// the only writer, so no serialization is needed.
    #[test]
    fn turn_lifecycle_writes_the_store_registry() {
        use crate::lockutil::LockOk;
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("s.db")).unwrap(),
        ));
        set_runs_store(store.clone());

        let sid = "tap-registry-roundtrip";
        turn_started(sid, "tui", 0, "");
        {
            let st = store.lock_ok();
            let live = st.runs_live(30).unwrap();
            let row = live
                .iter()
                .find(|r| r.session_id == sid)
                .expect("turn_started must register the run");
            assert_eq!(
                row.surface, "tui",
                "the surface travels with the row — 'which window is busy' is \
                 the question being answered"
            );
            assert_eq!(row.pid, std::process::id() as i64);
        }
        turn_ended(sid);
        let gone = !store
            .lock_ok()
            .runs_live(30)
            .unwrap()
            .iter()
            .any(|r| r.session_id == sid);
        assert!(
            gone,
            "turn_ended must deregister immediately, not wait \
                      for the heartbeat thread to notice"
        );
    }

    /// A pid that named a process a moment ago and names none now.
    fn a_dead_pid() -> u32 {
        let mut c = crate::shell::std_command("exit 0").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    fn gone_within(sid: &str, ms: u64) -> bool {
        let t = std::time::Instant::now();
        while t.elapsed() < std::time::Duration::from_millis(ms) {
            if snapshot(sid).is_none() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// 2026-10-04: three translation subagents showed "running" for up
    /// to 18 hours. Each was a delegate child killed at its 900 s
    /// timeout; its events reached the gateway through the relay, the
    /// gateway mirrored the turn and heartbeated it itself, and the
    /// `turn-ended` that would have cleared it was never sent.
    #[test]
    fn a_mirrored_turn_ends_when_the_process_running_it_dies() {
        let sid = "mirror-owner-dies";
        turn_started_by(sid, "subagent", 0, "", Some((a_dead_pid(), hostname())));
        assert!(
            gone_within(sid, 3000),
            "a mirrored turn whose owner is gone must end here"
        );
    }

    #[test]
    fn a_mirrored_turn_with_a_live_owner_stays_live() {
        let sid = "mirror-owner-lives";
        turn_started_by(
            sid,
            "subagent",
            0,
            "",
            Some((std::process::id(), hostname())),
        );
        assert_eq!(
            snapshot(sid).and_then(|s| s.owner_pid),
            Some(std::process::id()),
            "the snapshot names the owner it watches"
        );
        assert!(!gone_within(sid, 300), "a live owner keeps its turn");
        turn_ended(sid);
    }

    /// A pid means nothing off its own host, so a turn relayed from
    /// another machine is not watched (it lives until its turn-ended).
    #[test]
    fn a_turn_mirrored_from_another_host_is_not_watched() {
        let sid = "mirror-other-host";
        turn_started_by(
            sid,
            "subagent",
            0,
            "",
            Some((a_dead_pid(), "elsewhere".into())),
        );
        assert!(
            !gone_within(sid, 300),
            "another host's pid must not be probed here"
        );
        turn_ended(sid);
    }

    /// The watcher belongs to ONE turn: a later turn in the same
    /// session, started before the watcher's next look, is not ended
    /// on the dead owner's account.
    #[test]
    fn a_watcher_never_ends_the_next_turn() {
        let sid = "mirror-next-turn";
        turn_started_by(sid, "subagent", 0, "", Some((a_dead_pid(), hostname())));
        turn_started(sid, "desktop", 0, "");
        assert!(
            !gone_within(sid, 300),
            "the desktop's turn outlives the dead child's watcher"
        );
        turn_ended(sid);
    }

    /// A snapshot is removed by `turn_ended` and by nothing else, so a
    /// turn that dies without reaching it — a panic, a killed child, a
    /// gateway that lost the process writing the session — leaves one
    /// behind forever. Readers that treat "a snapshot exists" as "a
    /// live turn owns this transcript" then wait on a corpse; the
    /// attached-stream follow tick is exactly such a reader, which is
    /// how "it stopped streaming and looked hung" comes back.
    ///
    /// So freshness has to be askable, and every event has to refresh
    /// it.
    #[test]
    fn a_snapshot_nothing_has_touched_goes_stale() {
        let sid = "stale-snapshot-probe";
        turn_started(sid, "test", 0, "");
        assert!(
            snapshot_fresh(sid, std::time::Duration::from_secs(60)).is_some(),
            "a turn that just started is live by any threshold"
        );
        assert!(
            snapshot_fresh(sid, std::time::Duration::from_nanos(1)).is_none(),
            "and the same snapshot is stale under a threshold it cannot meet"
        );
        // An event refreshes it: a turn mid-tool-call is quiet, not dead.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let before = snapshot(sid).map(|s| s.last_event_at).unwrap_or(0);
        publish_stream(sid, &StreamEvent::Delta("tok".into()));
        let after = snapshot(sid).map(|s| s.last_event_at).unwrap_or(0);
        assert!(
            after > before,
            "every event must restamp the snapshot: {before} -> {after}"
        );
        turn_ended(sid);
        assert!(
            snapshot_fresh(sid, std::time::Duration::from_secs(60)).is_none(),
            "and a finished turn has no snapshot at all"
        );
    }
}
