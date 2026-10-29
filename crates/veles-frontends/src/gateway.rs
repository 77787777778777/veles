//! Gateway core — the message bus platform adapters plug into.
//!
//! Design (USER decisions baked in):
//!   - telegram = reference Rust adapter (teloxide, crates/veles-telegram)
//!   - irc = Rust adapter too (crates/veles-irc) — the original "Tcl
//!     adapters via tclcurl" plan was superseded when IRC grew taps,
//!     DCC-free media notes and owner commands
//!   - the gateway is a single binary; adapters are spawned tasks that
//!     translate platform events into `InboundMessage` and push replies
//!     back through `send`.
//!
//! The gateway owns one agent session per (platform, chat) pair, exactly
//! like hermes' gateway session routing (session_context.py). Display
//! semantics follow hermes' display.platforms config: streaming off,
//! reasoning shown as a blockquote-style prefix, progress edits cleaned.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use veles_core::agent::{Agent, TurnResult};
use veles_core::config::load;
use veles_core::hooks::HookBus;
use veles_core::lockutil::{LockOk, RwLockOk};
use veles_core::providers::{ChatMessage, StreamEvent};
use veles_core::store::Store;
use veles_core::tools;

pub use veles_core::bus::*;

pub struct Gateway {
    inbound_rx: tokio::sync::mpsc::UnboundedReceiver<InboundMessage>,
    core: Arc<GatewayCore>,
}

/// The running gateway's core, parked process-wide alongside the bus.
/// The desktop wake owner asks it whether a session is CHAT-BACKED, so
/// it never claims a wake that belongs in a chat (`GatewayBus` alone
/// cannot answer that).
static GLOBAL_CORE: std::sync::OnceLock<std::sync::RwLock<Option<Arc<GatewayCore>>>> =
    std::sync::OnceLock::new();

pub fn set_global_core(core: Arc<GatewayCore>) {
    // The ask relay's durable steer dock writes to this core's store.
    veles_core::ask::set_steer_store(core.store.clone());
    // Background completion events persist across a process restart.
    veles_core::delegate::set_bg_store(core.store.clone());
    // The cron scheduler's serialization seam (Stage 5): a session
    // wakeup borrows the OWNING surface's turn lock across its turn,
    // and for chat-backed sessions that surface is this gateway. The
    // core is parked globally at the same moment, so the runner joins
    // the registry here rather than at another point that could drift.
    veles_core::cron::register_session_runner(Arc::new(GatewaySessionRunner {
        core: Arc::clone(&core),
    }));
    // The same chats, listed and nudged for session mail.
    veles_core::presence::register(Arc::new(GatewaySessionRunner {
        core: Arc::clone(&core),
    }));
    if let Ok(mut w) = GLOBAL_CORE
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
    {
        *w = Some(core);
    }
}

// ── the gateway owner ────────────────────────────────────────────────

/// The gateway as a wake owner: chat-backed sessions only.
pub struct GatewayWakeOwner {
    pub core: Arc<GatewayCore>,
    pub bus: GatewayBus,
}

impl veles_core::session_wake::WakeOwner for GatewayWakeOwner {
    fn own_sid(&self, session_id: &str) -> bool {
        self.core.owns_session(session_id)
    }

    fn admit_and_run(
        &self,
        event: veles_core::session_wake::WakeEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let core = self.core.clone();
        let bus = self.bus.clone();
        Box::pin(async move {
            if let Err(e) = core.gateway_wake_one(&bus, &event).await {
                tracing::warn!("background-wake failed for {}: {e:#}", event.session_id);
            }
        })
    }
}

/// The gateway as a turn-owning surface, for cron session wakeups.
/// `owns_session` is the chat-backed predicate the desktop's wake owner
/// already defers to, so the two surfaces cannot both claim a session.
struct GatewaySessionRunner {
    core: Arc<GatewayCore>,
}

impl veles_core::cron::SessionRunner for GatewaySessionRunner {
    fn owns_session(&self, session_id: &str) -> bool {
        self.core.owns_session(session_id)
    }

    fn try_lock_session(&self, session_id: &str) -> Option<veles_core::cron::SessionLockGuard> {
        let lock = self
            .core
            .turn_lock(&self.core.session_lock_key(session_id)?);
        // `try_lock_owned`, not `try_lock`: the guard travels back to
        // the scheduler, which holds it across an `.await`, and a
        // borrowed `MutexGuard` cannot outlive this call.
        match lock.try_lock_owned() {
            Ok(g) => Some(veles_core::cron::SessionLockGuard::new(g)),
            Err(_) => None, // a turn is running; its drain covers the queue
        }
    }

    fn kick_queue(&self, session_id: &str) {
        self.core.kick_parked_queue(session_id);
    }
}

/// The gateway's chats as sessions this process holds open (session
/// mail, `veles_core::presence`). A nudge hands the parked row back to
/// the inbound pipeline the way a cron kick does; a chat mid-turn takes
/// it at its own boundary.
impl veles_core::presence::PresenceSource for GatewaySessionRunner {
    fn open_sessions(&self) -> Vec<veles_core::presence::OpenSession> {
        self.core
            .meta
            .lock_ok()
            .iter()
            .map(|(sid, (platform, _))| veles_core::presence::OpenSession {
                session_id: sid.clone(),
                surface: platform.clone(),
            })
            .collect()
    }

    fn nudge(&self, session_id: &str) -> bool {
        let Some(key) = self.core.session_lock_key(session_id) else {
            return false;
        };
        // Mid-turn: the turn's own drain runs it next.
        if self.core.turn_lock(&key).try_lock().is_err() {
            return true;
        }
        self.core.kick_parked_queue(session_id);
        true
    }
}

/// Drains a session's parked queue once a slash command has released
/// the session's turn lock.
///
/// Only a turn's end drains `session_queue` (R4.6). A slash command
/// takes the same lock with `lock().await` and runs no turn, so a plain
/// line that parked behind it ("queued #N — a turn is running; it runs
/// next") stayed parked until the sender's NEXT turn, and then ran after
/// it, out of order (found 2026-10-13). Declared BEFORE the guard so it
/// drops AFTER it: the lock is free when the row is kicked back into the
/// inbound pipeline, the same release-then-reinject order as
/// `gateway_wake_one`. A plain-chat turn needs none of this — its own
/// boundary drains — so the plain path makes none.
struct DrainAfterCommand<'a> {
    core: &'a GatewayCore,
    turn_key: String,
}

impl Drop for DrainAfterCommand<'_> {
    fn drop(&mut self) {
        let sid = self.core.session_ids.lock_ok().get(&self.turn_key).cloned();
        let Some(sid) = sid else {
            return;
        };
        let waiting = self
            .core
            .store
            .lock_ok()
            .queue_items(&sid)
            .map(|rows| !rows.is_empty())
            .unwrap_or(false);
        if waiting {
            self.core.kick_parked_queue(&sid);
        }
    }
}

/// The parked core, for call sites outside the gateway's own async
/// context (the ask dock's durable-steer path). None when no gateway
/// booted in this process.
pub fn global_core_arc() -> Option<Arc<GatewayCore>> {
    GLOBAL_CORE
        .get()
        .and_then(|c| c.read().ok())
        .and_then(|g| g.as_ref().map(std::clone::Clone::clone))
}

pub fn gateway_owns_session(session_id: &str) -> bool {
    GLOBAL_CORE
        .get()
        .and_then(|c| c.read().ok())
        .and_then(|g| Some(g.as_ref()?.clone()))
        .is_some_and(|core| core.owns_session(session_id))
}

/// One progress line per WINDOW of tool calls, not one per call.
///
/// The #coven flood (2026-09-12): ~30 `⚙ tool_call…` PRIVMSGs between a
/// question and its answer. `tool_call` is the deferred-tool wrapper, so
/// all thirty lines were mutually indistinguishable AND named nothing
/// real. This replaces that with `⚙ 6 calls: terminal ×4, …` — the
/// property the marker exists for (a long turn says something is
/// happening) at a sixth of the noise — and caps summaries per turn so
/// loudness is not config-dependent: the budget bites on every
/// connector whatever `verbosity` says.
///
/// The BUDGET is why the tail flush exists as a separate step. Without
/// it, a turn whose calls do not fill a whole window would narrate
/// NOTHING (and a multi-round turn whose last round chained a short
/// run of tools the same). Before the reply goes out, whatever the
/// window still holds is sent and the budget closes over it.
#[derive(Default)]
pub(crate) struct ProgressWindow {
    /// Tool names first-seen this window, so the summary reads in call
    /// order rather than hash order.
    order: Vec<String>,
    /// Calls per name since the last summary line (the window).
    counts: std::collections::HashMap<String, usize>,
    /// Summary lines sent this turn.
    lines_sent: usize,
    /// Total calls ever noted on this window (across resets) — the
    /// numerator for "nothing new since the last line".
    total: u64,
    /// Total calls covered by the last line sent.
    line_total: u64,
}

impl ProgressWindow {
    /// The name that goes in the summary: deferred calls resolve to the
    /// INNER target, so `tool_call` with `{"name":X}` names X — the
    /// deferred/literal split has exactly one definition, here.
    fn resolve_progress_name(name: &str, args: &str) -> String {
        if name == "tool_call" {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(inner) = v.get("name").and_then(|x| x.as_str()) {
                    if !inner.is_empty() {
                        return inner.to_string();
                    }
                }
            }
        }
        name.to_string()
    }

    /// A call happened; fold it into the window. The summary line for a
    /// FULL window comes back when the count reaches the window size —
    /// a line per six calls, so the narration keeps up with a long
    /// tool chain without narrating any single call.
    fn note(&mut self, name: &str, args: &str) -> Option<String> {
        let name = Self::resolve_progress_name(name, args);
        if !self.counts.contains_key(&name) {
            self.order.push(name.clone());
        }
        *self.counts.entry(name).or_insert(0) += 1;
        self.total += 1;
        self.line(false)
    }

    /// The line for everything since the last one was sent, or None
    /// when there is nothing new. `tail` is the reply-side flush: what
    /// the window still holds closes and counts there, even a partial
    /// one, because no more calls are coming.
    fn line(&mut self, tail: bool) -> Option<String> {
        let fresh = self.total - self.line_total;
        if fresh == 0 {
            return None;
        }
        if !tail && fresh < PROGRESS_WINDOW_SIZE {
            return None;
        }
        let parts = self
            .order
            .iter()
            .map(|n| format!("{n} ×{}", self.counts.get(n).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(", ");
        self.order.clear();
        self.counts.clear();
        self.line_total = self.total;
        let verb = if fresh == 1 { "call" } else { "calls" };
        match self.lines_sent {
            // Within budget: the summary narrates the window.
            n if n < PROGRESS_WINDOW_BUDGET => {
                self.lines_sent += 1;
                Some(format!("⚙ {fresh} {verb}: {parts}"))
            }
            // Exactly at the budget: say so ONCE, then go silent —
            // the cap is a brake, not a slower flood, so its own text
            // must not repeat.
            PROGRESS_WINDOW_BUDGET => {
                self.lines_sent += 1;
                Some(format!(
                    "⚙ {fresh} more {verb} (not narrated — the {PROGRESS_WINDOW_BUDGET}-line progress budget is spent); work continues"
                ))
            }
            // Past it: nothing more is sent. Calls still count, so a
            // later note does not resurrect narration.
            _ => None,
        }
    }
}

/// How many calls one summary line covers: the per-line throttling
/// granularity, set at the whole-turn budget's preferred line count
/// just below, so the two knobs stay proportionate.
const PROGRESS_WINDOW_SIZE: u64 = 6;

/// How many lines a turn's tool narration may spend before it turns
/// into a summary of itself. Chosen small: it bounds the gap between a
/// caller's question and its answer on a connector, and a turn that
/// spent MORE than this many tool windows has outlived narration.
const PROGRESS_WINDOW_BUDGET: usize = 3;

/// Per-session progress windows, so two turns cannot leak a summary into
/// each other's narration — two chats, or since 2026-10-13 two people's
/// sessions in ONE channel, whose turns now run at once. Keyed on the
/// turn-lock key. Owned by ONE `GatewayCore` (`progress`), not by the
/// process.
///
/// It was a process-global `static`, which is indistinguishable from
/// this in production — one core per process — and wrong everywhere
/// else. `cargo test` runs tests in parallel threads in one process and
/// every gateway test here sends to `#chan`, so two of them shared a
/// window; `tail_progress` REMOVES the entry, so one test ending its
/// turn handed another a fresh budget mid-flight and an extra ⚙ line.
/// That is the CI failure of 2026-09-17 ("30 calls must be 4 lines, not
/// 30" with five), green on every developer box and red on a
/// differently-scheduled runner. Its siblings — `turn_locks`,
/// `sessions`, `turn_permits` — have always lived on the core; this is
/// now where it belongs too.
pub(crate) type ProgressWindows =
    std::sync::Mutex<std::collections::HashMap<String, ProgressWindow>>;

/// Fold a tool call into the window for `turn_key`. A FULL window
/// returns its line here; anything else stays parked for the tail
/// flush before the reply.
fn note_progress(w: &ProgressWindows, turn_key: &str, name: &str, args: &str) -> Option<String> {
    w.lock_ok()
        .entry(turn_key.to_string())
        .or_default()
        .note(name, args)
}

/// The window's line as the turn ends — even a partial window, because
/// no more calls are coming — and the entry goes away so the next turn
/// starts a fresh, freshly-budgeted window.
fn tail_progress(w: &ProgressWindows, turn_key: &str) -> Option<String> {
    let mut w = w.lock_ok();
    let line = w.get_mut(turn_key).and_then(|window| window.line(true));
    w.remove(turn_key);
    line
}

/// Which session the gateway last spoke FOR in each shared room, as
/// `platform:chat_id` → turn-lock key. See [`Voice`].
pub(crate) type RoomSpeakers = std::sync::Mutex<HashMap<String, String>>;

/// One turn's outbound channel into its chat. In a shared room it
/// addresses the session's owner (`nick: …`) whenever the room's
/// previous line spoke for a different session.
///
/// USER (2026-10-13), as the sessions in one channel stopped queueing
/// behind each other: "make it address the session owner when it
/// changes context … so that it's less confusing, because it is a
/// shared stream". Two people's turns interleave in #flashsupport now,
/// and an unaddressed line could belong to either.
///
/// Only the lines every connector shows count: `Step` and `Reply` with
/// text. `Progress` (typing, streaming partials, `⚙` summaries) is shown
/// or dropped per adapter, and a prefix spent on a dropped line would
/// leave the next visible one unaddressed. The decision and the send
/// happen under one lock, so two turns cannot decide in one order and
/// reach the wire in the other.
#[derive(Clone)]
struct Voice {
    tx: tokio::sync::mpsc::UnboundedSender<OutboundMessage>,
    room: Option<Arc<VoiceRoom>>,
    /// The owner's chats that joined this session from elsewhere
    /// (`/join`): each gets a copy of what the session says.
    tees: Vec<Tee>,
}

/// A copy of a session's lines for the owner, who joined it from
/// another chat (`/join`, `veles_core::join`): the Steps and Replies
/// everyone in the session sees, each headed with whose session it is.
#[derive(Clone)]
struct Tee {
    tx: tokio::sync::mpsc::UnboundedSender<OutboundMessage>,
    chat_id: String,
    label: String,
}

impl Tee {
    /// One line into the owner's chat.
    fn send(&self, kind: MessageKind, text: &str) {
        let _ = self.tx.send(OutboundMessage {
            chat_id: self.chat_id.clone(),
            reply_to: None,
            text: format!("{} {text}", self.label),
            reasoning: None,
            provider: String::new(),
            model: String::new(),
            kind,
            ask: None,
            meta: Default::default(),
        });
    }
}

/// Where a [`Voice`] speaks and for whom. Absent in a DM, on a surface
/// that is not a connector, or when the sender has no name to address.
struct VoiceRoom {
    speakers: Arc<RoomSpeakers>,
    room: String,
    speaker: String,
    name: String,
}

impl Voice {
    /// Hand `out` to the adapter; false when it has gone away.
    fn send(&self, mut out: OutboundMessage) -> bool {
        // The owner's copy, of the lines every connector shows, before the
        // room's addressing touches the text.
        if matches!(out.kind, MessageKind::Step | MessageKind::Reply) && !out.text.trim().is_empty()
        {
            for t in &self.tees {
                t.send(out.kind, &out.text);
            }
        }
        let Some(r) = &self.room else {
            return self.tx.send(out).is_ok();
        };
        if !matches!(out.kind, MessageKind::Step | MessageKind::Reply) || out.text.trim().is_empty()
        {
            return self.tx.send(out).is_ok();
        }
        let mut last = r.speakers.lock_ok();
        let switched = last.get(&r.room).is_some_and(|prev| *prev != r.speaker);
        if switched && !addressed_to(&out.text, &r.name) {
            out.text = format!("{}: {}", r.name, out.text);
        }
        last.insert(r.room.clone(), r.speaker.clone());
        self.tx.send(out).is_ok()
    }
}

/// The owner, joined from one of their chats to someone else's session
/// (`/join`): where their lines go, and where that session's come back.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct JoinBinding {
    pub(crate) sid: String,
    /// The owner as that session's chat and its model will call them.
    owner_name: String,
    /// The owner's subject, the only one who may answer their turns'
    /// questions.
    owner_subject: String,
    owner_platform: String,
    owner_route: Option<String>,
    owner_chat: String,
    /// What heads each line mirrored to the owner: `[alice in #chan]`.
    label: String,
    /// The gateway's per-sender route and last observed identity. These
    /// restore the original session id; agent_for_sender re-resolves
    /// rights from the live ACL rather than trusting the stored role.
    #[serde(default)]
    target_key: String,
    #[serde(default)]
    target_platform: String,
    #[serde(default)]
    target_chat: String,
    #[serde(default)]
    target_principal: Option<veles_core::acl::Principal>,
    #[serde(default)]
    target_sender: Option<veles_core::acl::SenderContext>,
    /// Desktop's opaque id. Execution and restart use this same shared
    /// gateway binding; the client keeps its own return-view mapping.
    #[serde(default)]
    desktop: Option<DesktopJoinMetadata>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct DesktopJoinMetadata {
    pub(crate) join_id: String,
}

/// A session the owner may join.
#[derive(Clone, Debug)]
struct Joinable {
    sid: String,
    platform: String,
    chat: String,
    /// What the conversation calls the session's person.
    speaker: String,
    direct: bool,
}

impl Joinable {
    fn label(&self) -> String {
        if self.direct {
            format!("[{} on {}]", self.speaker, self.platform)
        } else {
            format!("[{} in {}]", self.speaker, self.chat)
        }
    }
}

/// Does `text` already open by addressing `name` (`name: …`, `name, …`)?
/// The model often does it itself, and a second prefix reads as a stutter.
fn addressed_to(text: &str, name: &str) -> bool {
    let t = text.trim_start();
    t.len() > name.len()
        && t.is_char_boundary(name.len())
        && t[..name.len()].eq_ignore_ascii_case(name)
        && matches!(t[name.len()..].chars().next(), Some(':' | ','))
}

/// The name a line for this sender is addressed to: the nick on the wire
/// first (IRC's `sender` reads `nick (account)` for a logged-in user, and
/// a parked or woken turn has no `sender` at all), then the handle the
/// platform shows, then `sender` without its account suffix.
fn address_name(sender: &str, ctx: &veles_core::acl::SenderContext) -> Option<String> {
    let from_mask = ctx
        .mask
        .as_deref()
        .and_then(|m| m.split('!').next())
        .filter(|n| !n.is_empty());
    from_mask
        .or(ctx.handle.as_deref())
        .or_else(|| sender.split(" (").next())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from)
}

/// Send one intermediate assistant message to a chat as a `Step`. Empty
/// content is dropped — a tool round that said nothing contributes no
/// message — and the provider/model footer is left empty so adapters do
/// not decorate every step with the runtime line they reserve for the
/// final reply.
fn send_step(tx: &Option<Voice>, chat_id: &str, content: String, reasoning: Option<String>) {
    if content.trim().is_empty() {
        return;
    }
    if let Some(tx) = tx {
        let _ = tx.send(OutboundMessage {
            chat_id: chat_id.to_string(),
            reply_to: None,
            text: content,
            reasoning,
            provider: String::new(),
            model: String::new(),
            kind: MessageKind::Step,
            ask: None,
            meta: Default::default(),
        });
    }
}

/// A reply to `msg` that a threaded platform posts UNDER the message it
/// answers. `OutboundMessage::plain` leaves `reply_to` empty, and on a
/// Bluesky feed that sent every command answer (`/tcl` included) to the
/// conversation's ROOT post instead of the mention that asked — the
/// level dd71734 fixed for turns only. Flat platforms ignore `reply_to`.
fn reply_plain(
    msg: &InboundMessage,
    text: impl Into<String>,
    kind: MessageKind,
) -> OutboundMessage {
    let mut o = OutboundMessage::plain(&msg.chat_id, text, kind);
    o.reply_to = msg.msg_id.clone();
    o
}

/// The commands a non-owner may run on a PUBLIC feed, where the answer
/// is a public post (USER 2026-10-06: the owner keeps every command,
/// mutuals get these). `/nodes`, `/status`, `/usage`, `/acl` and the
/// rest describe the pool or the sender's standing — fine in a DM,
/// a disclosure on a feed.
const PUBLIC_FEED_COMMANDS: &[&str] = &["tcl", "eval", "gen", "generate", "help"];

/// What a non-owner's `/help` says on a public feed: only what it may run.
const PUBLIC_FEED_HELP: &str = "On this public feed: /tcl <code> runs Tcl (the full result is \
linked), /gen <prompt> generates an image, /help shows this. Anything else: just write to me.";

/// Refuse a slash command a non-owner sent on a public feed, unless it is
/// one of [`PUBLIC_FEED_COMMANDS`]. `None` = let it through.
fn public_feed_refusal(
    platform: &str,
    text: &str,
    principal: &veles_core::acl::Principal,
) -> Option<String> {
    if !veles_core::bus::is_public_feed(platform) || principal.role == veles_core::acl::Role::Owner
    {
        return None;
    }
    let cmd = text.trim_start().strip_prefix('/')?;
    let head = cmd.split_whitespace().next().unwrap_or("");
    if PUBLIC_FEED_COMMANDS.contains(&head) {
        return None;
    }
    // A role held by ticket asks for its one command here; the raised
    // command comes back through this check as the role it was granted.
    if head == "sudo" && principal.ticket_role.is_some() {
        return None;
    }
    Some(format!(
        "`/{head}` is not available here: on a public feed its answer would be a public post. \
         /tcl, /gen and /help are.{}",
        veles_core::ticketed::sudo_hint(principal, veles_core::acl::Cap::All)
    ))
}

/// One in-flight turn, counted against its subject's concurrency limit.
///
/// Held for the whole of `handle()` — including the drain loop, so the
/// queued rows a turn picks up run under the permit that admitted the
/// message rather than claiming their own. Dropping it is what frees the
/// slot, which is why it is a guard and not a counter the caller has to
/// remember to decrement: a leaked slot is worse than the race this
/// replaced, because it is permanent and invisible.
#[derive(Debug)]
struct TurnSlot {
    subject: String,
    counts: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for TurnSlot {
    fn drop(&mut self) {
        let mut map = self.counts.lock_ok();
        let Some(n) = map.get_mut(&self.subject) else {
            return;
        };
        *n = n.saturating_sub(1);
        // Zero means nobody is holding a turn for this subject, and the
        // entry is the only thing keeping the key alive: a gateway in six
        // channels is a map that grows with every sender who ever spoke.
        if *n == 0 {
            map.remove(&self.subject);
        }
    }
}

/// Everything a turn needs, shared by value across spawned handlers.
///
/// Why this exists (M11): the inbound loop used to `await` `handle()`
/// inline, so ONE chat could stall every platform. With approvals the
/// stall became unbounded — a turn parked on a human decision would
/// silence the whole gateway. Messages are now handled in spawned tasks
/// and ordering is preserved PER SESSION by `turn_locks`, not globally.
pub struct GatewayCore {
    bus: GatewayBus,
    /// One agent per (platform, chat) session. The map guard is a std
    /// mutex (never held across an await); the AGENT guard is a tokio
    /// mutex because it IS held across `run_turn().await` and must be
    /// Send now that turns run in spawned tasks (pitfall 21 / the same
    /// reasoning documented in desktop_api.rs).
    /// `Arc<Agent>`, not `Arc<Mutex<Agent>>`: `run_turn` takes `&self`,
    /// and arrival order within one session is kept by `turn_lock` — which
    /// is where serialization belongs. Locking the agent for a turn's whole
    /// life additionally blocked every command typed during it.
    pub(crate) sessions: Mutex<HashMap<String, Arc<Agent>>>,
    /// Per-SESSION serialization, keyed on the session key
    /// (`session_key_for`): one person's messages still run in arrival
    /// order; different sessions run concurrently, in one channel too.
    ///
    /// It was keyed on the CHAT, from when one chat was one session, and
    /// stayed that way after a shared channel became one session per
    /// sender. Everyone in #flashsupport then waited on one lock: "a" got
    /// "queued #1 — a turn is running; it runs next" behind the owner's
    /// forty-minute turn (2026-10-13), and the promise was false too: the
    /// row parked in a's session, and the owner's turn drains only its
    /// own. USER: "the sessions should definitely run concurrently".
    turn_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Per-session tool-progress windows for THIS core's turns. `Arc`
    /// because the stream-event closure a turn hands to `run_turn`
    /// outlives the borrow of `self` that made it — the same reason
    /// `turn_permits` below is one. See `ProgressWindows` for why this
    /// is not the process-global it used to be.
    progress: Arc<ProgressWindows>,
    /// Who each shared room last heard from — see [`Voice`]. `Arc` for
    /// the same closure reason as `progress`.
    speakers: Arc<RoomSpeakers>,
    /// Per-SUBJECT in-flight turn permits (`[acl.limits]
    /// max_concurrent_turns`).
    ///
    /// `turn_locks` above already serializes one session, which is why
    /// this is a different question: one person addressing the bot in
    /// six channels at once has six sessions and takes six turn locks,
    /// and none of them sees the others. That is the shape that turns a rate limit into a
    /// parallelism problem — twenty simultaneous turns, each entitled
    /// to `max_iterations` tool rounds.
    ///
    /// The permit is REFUSED, never queued: a chat message that
    /// silently waits looks like the bot ignoring you, and the honest
    /// answer ("you already have N running") is one the sender can act
    /// on.
    ///
    /// A COUNT of in-flight turns per subject, not a semaphore: the
    /// limit is compared against the holders that exist, read at admit
    /// time, so lowering `[acl.limits] max_concurrent_turns` under a
    /// running flood takes effect on the next message. The semaphore
    /// this replaced was REBUILT when the limit changed, and permits
    /// held against the old one were invisible to the new one — so the
    /// knob admitted one more turn than the limit it had just been
    /// lowered to, which is precisely when somebody lowers it.
    ///
    /// `Arc` because the guard handed to a turn decrements on drop and
    /// outlives the borrow of `self` that created it.
    turn_permits: Arc<Mutex<HashMap<String, usize>>>,
    /// SESSION KEY (`platform:chat[:subject]`) → session id, so
    /// `/cancel` can find the running turn WITHOUT locking the agent
    /// (which the turn is holding) or the chat lock (which the turn is
    /// also holding). The session id lives inside the Agent, and
    /// reading it there is exactly the wait this map exists to avoid.
    ///
    /// Keyed on the session and not the CHAT since 2026-09-09: with one
    /// entry per chat the map held whichever session was created last,
    /// so in a shared channel `/cancel` stopped a stranger's turn — or
    /// reported success and stopped nothing, while the caller's own
    /// turn ran on. An owner's session key IS the plain chat key, so
    /// the owner's entry is unchanged.
    session_ids: Mutex<HashMap<String, String>>,
    /// session id → (platform, chat_id): the reverse of `session_ids`,
    /// so a completion event naming a session can find its chat.
    /// Hydrated in `agent_for_sender` exactly as the forward map is,
    /// so the two agree by construction — no second enumeration of
    /// "which platforms exist" to drift.
    meta: Mutex<HashMap<String, (String, String)>>,
    /// The owner, joined to someone else's session from one of their own
    /// chats (`/join`, `veles_core::join`): the owner's turn key in that
    /// chat → the session their lines go into. One session per chat.
    joins: Mutex<HashMap<String, JoinBinding>>,
    /// Shared with every surface (the desktop server holds the same
    /// Arc). `pub(crate)` for the ask dock's durable-steer path.
    pub(crate) store: Arc<Mutex<Store>>,
    pub(crate) cfg: Arc<veles_core::config::VelesConfig>,
    /// True when this gateway was built from a config FILE, so the
    /// `[acl]` section is re-read from disk per message (hot reload).
    /// False for a gateway handed a config in memory, whose section is
    /// the only one there is.
    cfg_from_file: bool,
    /// ONE Tcl interpreter + hook bus per gateway process (slopdrop model:
    /// one bot, one interp, many frontends). All session agents share it.
    hooks: Arc<HookBus>,
}

impl Gateway {
    pub fn new() -> Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = Arc::new(load()?);
        // i18n: install the process locale before any user-facing text.
        let _ = veles_core::i18n::init(cfg.tui.locale.as_deref());
        let home = veles_core::config::veles_home();
        let store = Arc::new(Mutex::new(Store::open(&home.join("state.db"))?));
        // S9: pairing codes resolve against this store process-wide.
        veles_core::acl::set_runtime_pairing_store(store.clone());
        // Shared plugins interpreter for the whole gateway process.
        let tcl = veles_core::tclrt::spawn(veles_core::config::config_dir())?;
        let hooks = HookBus::new(Some(tcl));
        let outbound = Arc::new(Mutex::new(HashMap::new()));
        let last_chat = Arc::new(Mutex::new(HashMap::new()));
        let state_path = home.join("gateway-state.json");
        // Restore the last-known chats so lifecycle notices survive a
        // restart (boot announcement, shutdown notice).
        if let Ok(raw) = std::fs::read_to_string(&state_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) {
                *last_chat.lock_ok() = map;
            }
        }
        Ok(Self {
            inbound_rx: rx,
            core: {
                let core = Arc::new(GatewayCore {
                    bus: {
                        let bus = GatewayBus {
                            inbound: tx,
                            outbound: outbound.clone(),
                            last_chat: last_chat.clone(),
                            state_path,
                        };
                        // Park the bus process-wide so the plugin bridge's
                        // `veles::send_outbound` can reach the adapters
                        // without holding a Gateway.
                        set_global_bus(bus.clone());
                        bus
                    },
                    sessions: Mutex::new(HashMap::new()),
                    turn_locks: Mutex::new(HashMap::new()),
                    joins: Mutex::new(HashMap::new()),
                    progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
                    speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
                    turn_permits: Arc::new(Mutex::new(HashMap::new())),
                    session_ids: Mutex::new(HashMap::new()),
                    meta: Mutex::new(HashMap::new()),
                    store,
                    cfg,
                    // A real gateway boots from config.toml, so the ACL is
                    // re-read from it per message.
                    cfg_from_file: veles_core::acl::config_file_present(),
                    hooks,
                });
                // The desktop wake owner asks "is this session
                // chat-backed?" through this — the wake for one must go
                // to the CHAT, not to whoever happened to run a turn.
                set_global_core(core.clone());
                core
            },
        })
    }

    pub fn bus(&self) -> GatewayBus {
        self.core.bus.clone()
    }

    /// The gateway's own store, for adapters that park a handle to it
    /// (the IRC scrollback search reads through `veles_irc::client::
    /// park_store`).
    pub fn store(&self) -> Arc<Mutex<Store>> {
        self.core.store.clone()
    }
}

/// `inbound.route` asked about one message (PLAN-extension-api.md E3):
/// `Some` to route (the message as the override answered it), `None` to
/// drop. The override may change the text; it may not change who sent
/// it or where (`points::INBOUND_IDENTITY`), because the ACL resolves the
/// sender after this point: such an answer is refused, with a note, and
/// the ORIGINAL message is routed. An answer that cannot be read routes
/// the original too.
async fn inbound_point(msg: InboundMessage) -> Option<InboundMessage> {
    let Ok(original) = serde_json::to_value(&msg) else {
        return Some(msg);
    };
    let refuse = |why: String| {
        tracing::warn!("{why}");
        veles_core::notes::record_plugin_note(&why, "the original message is routed");
    };
    let point = veles_core::points::INBOUND;
    match veles_core::services::ask(point, original.clone(), None).await {
        Ok(v) if v["action"] == "drop" => {
            tracing::info!(
                "`{point}` dropped a message from {} on {}",
                msg.sender,
                msg.platform
            );
            None
        }
        Ok(v) => {
            let got = v.get("message").cloned().unwrap_or(original.clone());
            if let Err(why) = veles_core::points::inbound_identity_kept(&original, &got) {
                refuse(format!(
                    "`{point}` tried to change who sent a message: {why}"
                ));
                return Some(msg);
            }
            match serde_json::from_value::<InboundMessage>(got) {
                Ok(m) => Some(m),
                Err(e) => {
                    refuse(format!(
                        "`{point}` answered a message that does not read: {e}"
                    ));
                    Some(msg)
                }
            }
        }
        Err(e) => {
            refuse(format!("`{point}` failed: {e}"));
            Some(msg)
        }
    }
}

/// The bottom of the inbound chain: `GatewayCore::handle_default`
/// borrowed for the chain's lifetime.
fn inbound_base<'a, 's: 'a>(
    core: &'s GatewayCore,
) -> std::sync::Arc<veles_core::layers::InboundBase<'a>> {
    std::sync::Arc::new(
        move |m: InboundMessage| -> veles_core::layers::BoxFuture<'a, ()> {
            Box::pin(core.handle_default(m))
        },
    )
}

impl GatewayCore {
    /// A core with no bus, no adapters, nothing live — purely the
    /// ownership maps, for the desktop-ownership split pin. ONLY tests
    /// need it; production code must never reach this shape.
    pub fn new_for_test(store: Arc<Mutex<Store>>) -> Arc<Self> {
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(Self {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(Mutex::new(HashMap::new())),
                last_chat: Arc::new(Mutex::new(HashMap::new())),
                state_path: std::env::temp_dir().join("veles-wake-test-state.json"),
            },
            sessions: Mutex::new(HashMap::new()),
            turn_locks: Mutex::new(HashMap::new()),
            joins: Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: Mutex::new(HashMap::new()),
            meta: Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(veles_core::config::VelesConfig::default()),
            cfg_from_file: false,
            hooks: veles_core::hooks::HookBus::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_join_test(store: Arc<Mutex<Store>>) -> Arc<Self> {
        let mut core = Self::new_for_test(store);
        // Tests need a real, offline model route and a curated ACL:
        // the legacy absent-ACL posture makes every sender an owner.
        let cfg = veles_core::config::VelesConfig {
            model: veles_core::config::ModelSection {
                provider: Some("test-provider".into()),
                default: Some("test-model".into()),
                ..Default::default()
            },
            custom_providers: vec![veles_core::config::ProviderEntry {
                name: "test-provider".into(),
                base_url: "http://127.0.0.1:0/v1".into(),
                api_mode: "chat_completions".into(),
                models: veles_core::config::ModelsSpec::List(vec!["test-model".into()]),
                ..Default::default()
            }],
            acl: veles_core::acl::AclConfig {
                default_role: Some("user".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        Arc::get_mut(&mut core).expect("fresh gateway core").cfg = Arc::new(cfg);
        core
    }

    #[cfg(test)]
    pub(crate) fn new_join_test_with_acl(store: Arc<Mutex<Store>>, role: &str) -> Arc<Self> {
        let mut core = Self::new_join_test(store);
        let mut cfg = (*core.cfg).clone();
        cfg.acl.default_role = Some(role.into());
        Arc::get_mut(&mut core).expect("fresh join fixture").cfg = Arc::new(cfg);
        core
    }

    #[cfg(test)]
    pub(crate) fn new_join_test_file_acl(store: Arc<Mutex<Store>>) -> Arc<Self> {
        let mut core = Self::new_join_test(store);
        Arc::get_mut(&mut core)
            .expect("fresh file-backed join fixture")
            .cfg_from_file = true;
        core
    }

    #[cfg(test)]
    pub(crate) fn test_sender(
        &self,
        platform: &str,
        chat: &str,
        sender: &str,
        ctx: veles_core::acl::SenderContext,
    ) -> anyhow::Result<String> {
        Ok(self
            .agent_for_sender(platform, chat, Some(sender), &ctx)?
            .session_id
            .clone())
    }

    #[cfg(test)]
    pub(crate) fn test_join_restore(self: &Arc<Self>) {
        self.restore_gateway_joins();
    }

    #[cfg(test)]
    pub(crate) fn test_join_live(&self, sid: &str) -> bool {
        self.sessions
            .lock_ok()
            .values()
            .any(|a| a.session_id == sid)
    }

    #[cfg(test)]
    pub(crate) fn test_join_key(&self, sid: &str) -> Option<String> {
        self.session_ids
            .lock_ok()
            .iter()
            .find(|(_, v)| v.as_str() == sid)
            .map(|(k, _)| k.clone())
    }

    /// The same map write `agent_for_sender` performs, for a session that
    /// is ALREADY in the store. Tests use it; production code gets the
    /// write from `agent_for_sender` itself.
    pub fn mark_chat_backed(&self, session_id: &str, platform: &str, chat_id: &str) {
        self.meta.lock_ok().insert(
            session_id.to_string(),
            (platform.to_string(), chat_id.to_string()),
        );
    }

    /// Does this gateway hold THIS session (created it as a chat-backed
    /// session)? The wake loop asks this: the gateway wakes its own
    /// sessions; the desktop server wakes the sessions it alone holds.
    pub fn owns_session(&self, session_id: &str) -> bool {
        self.meta.lock_ok().contains_key(session_id)
    }

    /// The (platform, chat_id) a completion event's session belongs
    /// to — the wake turn's delivery address.
    fn session_chat(&self, session_id: &str) -> Option<(String, String)> {
        self.meta.lock_ok().get(session_id).cloned()
    }

    /// The turn-lock key of a session this gateway holds, for the paths
    /// that know only its id (a cron run, a wake, a nudge): the key it is
    /// published under in `session_ids`, so they serialize with that
    /// person's chat turns and nobody else's. A session no longer
    /// published (`/new` rebound the key away from it) falls back to its
    /// plain chat key.
    fn session_lock_key(&self, session_id: &str) -> Option<String> {
        let published = self
            .session_ids
            .lock_ok()
            .iter()
            .find(|(_, sid)| sid.as_str() == session_id)
            .map(|(key, _)| key.clone());
        if published.is_some() {
            return published;
        }
        let (platform, chat_id) = self.session_chat(session_id)?;
        Some(format!("{platform}:{chat_id}"))
    }

    /// The channel a turn for `speaker`'s session speaks through — see
    /// [`Voice`]. `name` is who its lines are addressed to; a DM, a
    /// surface that is not a connector, or no name gets a plain channel.
    fn voice(
        &self,
        platform: &str,
        route: Option<&str>,
        chat_id: &str,
        direct: bool,
        speaker: &str,
        name: Option<String>,
    ) -> Option<Voice> {
        let tx = self.bus.outbound_channel(platform, route)?;
        Some(self.voice_on(tx, platform, chat_id, direct, speaker, name))
    }

    /// [`Self::voice`] over a channel the caller already resolved.
    fn voice_on(
        &self,
        tx: veles_core::bus::OutboundTx,
        platform: &str,
        chat_id: &str,
        direct: bool,
        speaker: &str,
        name: Option<String>,
    ) -> Voice {
        let room = name
            .filter(|_| !direct && veles_core::config::is_connector(platform))
            .map(|name| {
                Arc::new(VoiceRoom {
                    speakers: Arc::clone(&self.speakers),
                    room: format!("{platform}:{chat_id}"),
                    speaker: speaker.to_string(),
                    name,
                })
            });
        Voice {
            tx,
            room,
            tees: Vec::new(),
        }
    }

    // ── the owner, joined to someone else's session (`/join`) ──────────
    //
    // USER 2026-10-12/13 (`veles_core::join` has the decisions): the
    // owner can be a second person in a session that is not theirs. Their
    // plain lines in the chat they joined from go into that session as
    // join rows (`mail::send_join`: queued like mail, run with the
    // owner's rights) and show in its chat as theirs. What the session
    // says, and what its own person writes, comes back to the owner's
    // chat. Slash commands stay the owner's own.

    fn join_of(&self, owner_key: &str) -> Option<JoinBinding> {
        self.joins.lock_ok().get(owner_key).cloned()
    }

    /// The owner's chats that joined `sid` from somewhere other than its
    /// own chat: they get a copy of what it says.
    fn tees_for(&self, sid: &str) -> Vec<Tee> {
        let here = self.session_chat(sid);
        let joined: Vec<JoinBinding> = self
            .joins
            .lock_ok()
            .values()
            .filter(|b| b.sid == sid)
            .cloned()
            .collect();
        joined
            .into_iter()
            .filter(|b| here.as_ref() != Some(&(b.owner_platform.clone(), b.owner_chat.clone())))
            .filter_map(|b| {
                let tx = self
                    .bus
                    .outbound_channel(&b.owner_platform, b.owner_route.as_deref())?;
                Some(Tee {
                    tx,
                    chat_id: b.owner_chat,
                    label: b.label,
                })
            })
            .collect()
    }

    fn with_tees(&self, voice: Option<Voice>, sid: &str) -> Option<Voice> {
        voice.map(|mut v| {
            v.tees = self.tees_for(sid);
            v
        })
    }

    /// What starts a turn in a joined session, for the owner's chat: its
    /// own person's line, or mail. Never the owner's own line.
    fn mirror_turn_text(&self, agent: &Agent, body: &str) {
        let tees = self.tees_for(&agent.session_id);
        if tees.is_empty() {
            return;
        }
        let line = match veles_core::mail::parse_body(body) {
            Some((id, text)) => match self.store.lock_ok().mail_get(id).ok().flatten() {
                Some(r) if r.kind == veles_core::mail::KIND_JOIN => return,
                Some(r) => format!(
                    "✉ mail from {}: {text}",
                    r.from_title
                        .or(r.from_session)
                        .unwrap_or_else(|| "?".to_string())
                ),
                None => return,
            },
            None => {
                let who = veles_core::join::speaker_name(
                    &agent.session_id,
                    agent.standing_principal().as_deref(),
                )
                .unwrap_or_else(|| "?".to_string());
                format!("<{who}> {body}")
            }
        };
        for t in &tees {
            t.send(MessageKind::Step, &line);
        }
    }

    /// The sessions the owner may join: everyone's here but the owner's.
    fn joinable(&self) -> Vec<Joinable> {
        let agents: Vec<Arc<Agent>> = self.sessions.lock_ok().values().cloned().collect();
        let meta = self.meta.lock_ok().clone();
        let mut out: Vec<Joinable> = agents
            .into_iter()
            .filter_map(|a| {
                // No principal is an owner surface.
                let p = a.standing_principal()?;
                if p.role == veles_core::acl::Role::Owner {
                    return None;
                }
                let (platform, chat) = meta.get(&a.session_id)?.clone();
                let ctx = veles_core::acl::session_sender(&a.session_id);
                Some(Joinable {
                    speaker: veles_core::join::speaker_name(&a.session_id, Some(&p))
                        .unwrap_or_else(|| p.subject.clone()),
                    direct: ctx.as_deref().is_some_and(|c| c.direct),
                    sid: a.session_id.clone(),
                    platform,
                    chat,
                })
            })
            .collect();
        // Newest first: a session id starts with its creation time.
        out.sort_by(|a, b| b.sid.cmp(&a.sid));
        out.dedup_by(|a, b| a.sid == b.sid);
        out
    }

    fn join_store_key(owner_key: &str) -> String {
        format!("join:gateway:v1:{owner_key}")
    }

    fn another_owner_joined(&self, owner_key: &str, sid: &str) -> bool {
        self.joins
            .lock_ok()
            .iter()
            .any(|(key, b)| b.sid == sid && key != owner_key)
    }

    fn persist_join(&self, owner_key: &str, b: &JoinBinding) -> anyhow::Result<()> {
        let raw = serde_json::to_string(b)?;
        self.store
            .lock_ok()
            .meta_set(&Self::join_store_key(owner_key), &raw)
    }

    /// Desktop joins ride the gateway's existing conversation routing.
    /// The API bearer is the owner credential; this is not an A2A claim.
    pub(crate) fn desktop_join(
        self: &Arc<Self>,
        owner_key: &str,
        target: &str,
    ) -> anyhow::Result<veles_core::joinbind::Binding> {
        let (platform, chat_id) = self
            .session_chat(target)
            .ok_or_else(|| anyhow::anyhow!("no live chat-backed session {target} on this node"))?;
        let (key, agent) = self
            .sessions
            .lock_ok()
            .iter()
            .find(|(_, a)| a.session_id == target)
            .map(|(k, a)| (k.clone(), a.clone()))
            .ok_or_else(|| anyhow::anyhow!("session {target} is not live in this gateway"))?;
        let p = agent
            .standing_principal()
            .ok_or_else(|| anyhow::anyhow!("cannot join an owner session"))?;
        if matches!(
            p.role,
            veles_core::acl::Role::Owner | veles_core::acl::Role::Banned
        ) {
            anyhow::bail!("cannot join an owner or banned session");
        }
        let ctx = veles_core::acl::session_sender(target);
        let speaker = veles_core::join::speaker_name(target, Some(&p)).unwrap_or(p.subject.clone());
        let direct = ctx.as_deref().is_some_and(|c| c.direct);
        let label = if direct {
            format!("[{speaker} on {platform}]")
        } else {
            format!("[{speaker} in {chat_id}]")
        };
        let join_id = owner_key
            .strip_prefix("desktop:join:")
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("j{}", &uuid::Uuid::new_v4().simple().to_string()[..16]));
        let b = JoinBinding {
            sid: target.into(),
            owner_name: "owner".into(),
            owner_subject: "owner".into(),
            owner_platform: "desktop".into(),
            owner_route: None,
            owner_chat: join_id.clone(),
            label: label.clone(),
            target_key: key,
            target_platform: platform.clone(),
            target_chat: chat_id.clone(),
            target_principal: Some((*p).clone()),
            target_sender: ctx.as_deref().cloned(),
            desktop: Some(DesktopJoinMetadata {
                join_id: join_id.clone(),
            }),
        };
        // One owner-question route per session. A second view must not
        // leave its credential alive while asks go to a different owner.
        let stale = self.another_owner_joined(owner_key, target);
        if stale {
            anyhow::bail!("session {target} is already joined from another owner view; /leave that join first")
        }
        let binding = veles_core::joinbind::Binding {
            join_id,
            // Return-view labels are transport-only. They are filled by
            // the endpoint and saved by the client, not this gateway.
            from_instance: String::new(),
            from_session: String::new(),
            target_instance: String::new(),
            target_session: target.into(),
        };
        // Revalidate before installing the owner-question route, not
        // only when the first joined line arrives.
        self.live_join_target(&b)?;
        self.persist_join(owner_key, &b)
            .map_err(|e| anyhow::anyhow!("join could not be made restart-persistent: {e:#}"))?;
        self.joins.lock_ok().insert(owner_key.into(), b.clone());
        self.publish_join(&b);
        Ok(binding)
    }

    fn desktop_view(b: &JoinBinding) -> Option<veles_core::joinbind::Binding> {
        let d = b.desktop.as_ref()?;
        Some(veles_core::joinbind::Binding {
            join_id: d.join_id.clone(),
            // These are transport metadata for the client's registry,
            // not fields persisted by the target gateway.
            from_instance: String::new(),
            from_session: String::new(),
            target_instance: String::new(),
            target_session: b.sid.clone(),
        })
    }

    pub(crate) fn desktop_status(
        &self,
        join_id: &str,
        view: &str,
    ) -> anyhow::Result<veles_core::joinbind::Binding> {
        let owner_key = format!("desktop:join:{join_id}");
        let live = self
            .join_of(&owner_key)
            .ok_or_else(|| anyhow::anyhow!("join {join_id} is not active on this gateway"))?;
        if live.sid != view || live.desktop.as_ref().map(|d| d.join_id.as_str()) != Some(join_id) {
            anyhow::bail!("join {join_id} belongs to another session")
        }
        self.live_join_target(&live)?;
        Self::desktop_view(&live)
            .ok_or_else(|| anyhow::anyhow!("join {join_id} is not a desktop join"))
    }

    pub(crate) fn desktop_leave(
        &self,
        join_id: &str,
        view: &str,
    ) -> Option<veles_core::joinbind::Binding> {
        let owner_key = format!("desktop:join:{join_id}");
        let live = self.join_of(&owner_key)?;
        if live.sid != view || live.desktop.as_ref()?.join_id != join_id {
            return None;
        }
        let d = Self::desktop_view(&live)?;
        self.leave_command(&owner_key);
        // A development draft briefly stored a parallel join record.
        // Remove that stale mirror as well; it never authorizes a turn.
        let _ = self
            .store
            .lock_ok()
            .meta_delete(&format!("join:desktop:v1:{join_id}"));
        Some(d)
    }

    fn live_join_target(&self, b: &JoinBinding) -> anyhow::Result<Arc<Agent>> {
        let saved = b
            .target_principal
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("join has no sender identity to revalidate"))?;
        let ctx = b.target_sender.clone().unwrap_or_default();
        // No participant message may arrive while only the owner is using
        // this join. Do not trust the cached agent's old role or route.
        let current = self.resolve(saved.id.as_deref(), &ctx);
        if matches!(
            current.role,
            veles_core::acl::Role::Owner | veles_core::acl::Role::Banned
        ) || current.id != saved.id
            || current.subject != saved.subject
        {
            anyhow::bail!("join to {} is no longer valid under the current ACL", b.sid)
        }
        let key = self.session_key_for(
            &b.target_platform,
            &b.target_chat,
            current.id.as_deref(),
            &ctx,
        );
        if key != b.target_key {
            anyhow::bail!("join to {}: sender route changed; rejoin explicitly", b.sid)
        }
        // Check the cached session BEFORE agent_for_sender refreshes its
        // principal. Otherwise a cached owner is silently rewritten to
        // the resolved role and the revocation check below cannot see it.
        if let Some(cached) = self
            .sessions
            .lock_ok()
            .values()
            .find(|a| a.session_id == b.sid)
            .and_then(|a| a.standing_principal())
        {
            if matches!(
                cached.role,
                veles_core::acl::Role::Owner | veles_core::acl::Role::Banned
            ) {
                anyhow::bail!("join to {} is no longer valid for its session owner", b.sid)
            }
        }
        let agent = self.agent_for_sender(
            &b.target_platform,
            &b.target_chat,
            current.id.as_deref(),
            &ctx,
        )?;
        if agent.session_id != b.sid {
            anyhow::bail!("join to {}: sender route changed; rejoin explicitly", b.sid)
        }
        let current = agent
            .standing_principal()
            .ok_or_else(|| anyhow::anyhow!("cannot join an owner session"))?;
        if matches!(
            current.role,
            veles_core::acl::Role::Owner | veles_core::acl::Role::Banned
        ) {
            anyhow::bail!("join to {} is no longer valid under the current ACL", b.sid)
        }
        Ok(agent)
    }

    fn desktop_live_join(
        &self,
        join_id: &str,
        view: &str,
    ) -> anyhow::Result<(String, JoinBinding)> {
        let owner_key = format!("desktop:join:{join_id}");
        let live = self
            .join_of(&owner_key)
            .ok_or_else(|| anyhow::anyhow!("join {join_id} is not active on this gateway"))?;
        if live.sid != view {
            anyhow::bail!("join {join_id} belongs to another session")
        }
        // Leaving is cleanup of this exact owner/binding pair. It must
        // remain possible after the participant's ACL is revoked.
        Ok((owner_key, live))
    }

    pub(crate) fn desktop_input(
        &self,
        join_id: &str,
        view: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let (owner_key, live) = self.desktop_live_join(join_id, view)?;
        let msg = InboundMessage {
            platform: "desktop".into(),
            route: None,
            chat_id: join_id.into(),
            msg_id: None,
            dedup_id: None,
            sender: live.owner_name.clone(),
            sender_id: Some(live.owner_subject.clone()),
            sender_ctx: veles_core::acl::SenderContext::default(),
            text: text.into(),
            context: None,
        };
        self.join_line(
            &msg,
            &veles_core::acl::Principal::owner(),
            &owner_key,
            &live,
        )
    }

    /// Desktop sends joined turns through the gateway's existing join
    /// row. /leave is handled here; every other slash command is refused,
    /// rather than falling through to the ordinary desktop /steer path.
    pub(crate) fn desktop_command(
        &self,
        join_id: &str,
        view: &str,
        command: &str,
        attachments: &[String],
    ) -> anyhow::Result<Option<String>> {
        self.desktop_live_join(join_id, view)?;
        let line = command.trim_start();
        if line.starts_with('/') {
            if line == "/leave" {
                let Some(binding) = self.desktop_leave(join_id, view) else {
                    anyhow::bail!("joined session is no longer active")
                };
                return Ok(Some(format!(
                    "left {}; what you write here is your own session's again",
                    binding.target_session
                )));
            }
            anyhow::bail!(
                "commands stay on your own session; /leave returns from the joined session"
            );
        }
        let text = veles_core::media::with_attachments(command, attachments);
        self.desktop_input(join_id, view, &text)?;
        Ok(None)
    }

    // IRC, Telegram and Desktop restore from this one binding.
    pub(crate) fn restore_gateway_joins(self: &Arc<Self>) {
        let rows = self.store.lock_ok().meta_scan_prefix("join:gateway:v1:");
        for key in rows {
            let Some(raw) = self.store.lock_ok().meta_get(&key) else {
                continue;
            };
            let Ok(b) = serde_json::from_str::<JoinBinding>(&raw) else {
                continue;
            };
            let Some(owner_key) = key.strip_prefix("join:gateway:v1:") else {
                continue;
            };
            if let Some(d) = &b.desktop {
                if owner_key != format!("desktop:join:{}", d.join_id) {
                    tracing::warn!("join: desktop id does not match its owner key; not restoring");
                    continue;
                }
                // A development draft briefly duplicated this binding.
                // It is not an authority; remove the obsolete mirror.
                let _ = self
                    .store
                    .lock_ok()
                    .meta_delete(&format!("join:desktop:v1:{}", d.join_id));
            }
            if self.another_owner_joined(owner_key, &b.sid) {
                tracing::warn!(
                    "join: {} has another active owner view; not restoring duplicate",
                    b.sid
                );
                continue;
            }
            let ctx = b.target_sender.clone().unwrap_or_default();
            let Some(saved) = b.target_principal.as_ref() else {
                tracing::warn!(
                    "join: {} has no saved sender identity; not restoring",
                    b.sid
                );
                continue;
            };
            // Hydrate the original route before resolving the agent, as
            // agent_for_sender uses it to recover the original session id.
            // Do not restore a route that would replace a different live one.
            let key = self.session_key_for(
                &b.target_platform,
                &b.target_chat,
                saved.id.as_deref(),
                &ctx,
            );
            if key != b.target_key {
                tracing::warn!(
                    "join: sender route changed for {}; not restoring into another session",
                    b.sid
                );
                continue;
            }
            if self
                .session_ids
                .lock_ok()
                .get(&key)
                .is_some_and(|sid| sid != &b.sid)
            {
                tracing::warn!(
                    "join: sender route for {} is already owned; not restoring",
                    b.sid
                );
                continue;
            }
            self.session_ids.lock_ok().insert(key, b.sid.clone());
            self.meta.lock_ok().insert(
                b.sid.clone(),
                (b.target_platform.clone(), b.target_chat.clone()),
            );
            veles_core::acl::set_session_sender(&b.sid, ctx.clone());
            // Startup, status and every live input must apply the SAME
            // identity, live-ACL and route check. A weaker restore-only
            // sequence can otherwise install a route that live input
            // would immediately reject.
            match self.live_join_target(&b) {
                Ok(_) => {
                    self.joins.lock_ok().insert(owner_key.into(), b.clone());
                    self.publish_join(&b);
                }
                Err(e) => {
                    tracing::warn!("join: not restoring {}: {e:#}", b.sid);
                }
            }
        }
    }

    /// Tell the engine who has joined `b.sid` and where their turns ask.
    fn publish_join(&self, b: &JoinBinding) {
        veles_core::join::set(
            &b.sid,
            veles_core::join::Joined {
                owner_name: b.owner_name.clone(),
                asker: if b.owner_platform == "desktop" {
                    Arc::new(crate::desktop_api::DesktopAsker {
                        session_id: b.sid.clone(),
                    }) as Arc<dyn veles_core::ask::Asker>
                } else {
                    Arc::new(veles_core::ask::ChatAsker {
                        bus: self.bus.clone(),
                        platform: b.owner_platform.clone(),
                        chat_id: b.owner_chat.clone(),
                        // The owner alone answers what their turns ask.
                        sender_id: Some(b.owner_subject.clone()),
                        shared_audience: b.owner_platform == "irc" && b.owner_chat.starts_with('#'),
                    })
                },
            },
        );
    }

    /// `sid` lost a joined chat: another one of the owner's that is still
    /// joined takes over its questions, or nobody has joined it any more.
    fn unpublish_join(&self, sid: &str) {
        let still = self
            .joins
            .lock_ok()
            .values()
            .find(|b| b.sid == sid)
            .cloned();
        match still {
            Some(b) => self.publish_join(&b),
            None => veles_core::join::clear(sid),
        }
    }

    /// `/join [name|id]` from the owner's chat whose turn key is
    /// `owner_key`.
    fn join_command(
        &self,
        msg: &InboundMessage,
        principal: &veles_core::acl::Principal,
        owner_key: &str,
        rest: &str,
    ) -> String {
        let rest = rest.trim();
        let list = self.joinable();
        if rest.is_empty() {
            let mut out = match self.join_of(owner_key) {
                Some(b) => format!("joined: {} ({}); /leave to stop\n", b.label, b.sid),
                None => String::new(),
            };
            if list.is_empty() {
                out.push_str("nobody else's session is open on this node right now");
                return out;
            }
            out.push_str("sessions you can join (/join <name or id>):");
            for j in &list {
                out.push_str(&format!("\n  {}  {}", j.sid, j.label()));
            }
            return out;
        }
        let want = rest.to_lowercase();
        let exact: Vec<&Joinable> = list.iter().filter(|j| j.sid == rest).collect();
        let hits: Vec<&Joinable> = if exact.is_empty() {
            list.iter()
                .filter(|j| {
                    (rest.len() >= 4 && j.sid.starts_with(rest)) || j.speaker.to_lowercase() == want
                })
                .collect()
        } else {
            exact
        };
        let j = match hits.as_slice() {
            [] => {
                return format!(
                    "no session of \"{rest}\" is open here; /join lists the ones you can join"
                )
            }
            [one] => (*one).clone(),
            many => {
                let mut out = format!("\"{rest}\" names {} sessions; /join <id>:", many.len());
                for j in many {
                    out.push_str(&format!("\n  {}  {}", j.sid, j.label()));
                }
                return out;
            }
        };
        let owner_name =
            address_name(&msg.sender, &msg.sender_ctx).unwrap_or_else(|| principal.subject.clone());
        let b = JoinBinding {
            sid: j.sid.clone(),
            owner_name: owner_name.clone(),
            owner_subject: principal.subject.clone(),
            owner_platform: msg.platform.clone(),
            owner_route: msg.route.clone(),
            owner_chat: msg.chat_id.clone(),
            label: j.label(),
            target_key: self
                .session_ids
                .lock_ok()
                .iter()
                .find(|(_, sid)| sid.as_str() == j.sid)
                .map(|(k, _)| k.clone())
                .unwrap_or_default(),
            target_platform: j.platform.clone(),
            target_chat: j.chat.clone(),
            target_principal: self
                .sessions
                .lock_ok()
                .values()
                .find(|a| a.session_id == j.sid)
                .and_then(|a| a.standing_principal())
                .map(|p| (*p).clone()),
            target_sender: veles_core::acl::session_sender(&j.sid).as_deref().cloned(),
            desktop: None,
        };
        if let Err(e) = self.live_join_target(&b) {
            return format!("join refused: {e:#}");
        }
        // The engine has one owner-question route per session. Preserve
        // that invariant across both entry points, not just the Desktop.
        if self.another_owner_joined(owner_key, &b.sid) {
            return format!(
                "{} is already joined from another owner view; /leave that join first",
                b.label
            );
        }
        // One session per chat: a second /join moves this chat.
        let prev = self
            .joins
            .lock_ok()
            .insert(owner_key.to_string(), b.clone());
        self.publish_join(&b);
        if let Err(e) = self.persist_join(owner_key, &b) {
            if let Some(previous) = prev.as_ref() {
                self.joins
                    .lock_ok()
                    .insert(owner_key.to_string(), previous.clone());
                self.publish_join(previous);
            } else {
                self.joins.lock_ok().remove(owner_key);
                self.unpublish_join(&b.sid);
            }
            return format!("join failed; could not save restart state: {e:#}");
        }
        if let Some(prev) = prev.filter(|p| p.sid != b.sid) {
            self.unpublish_join(&prev.sid);
        }
        tracing::info!(
            "join: {} joined {} from {}",
            principal.subject,
            b.sid,
            owner_key
        );
        format!(
            "joined {label} ({sid}). What you write here goes into {who}'s session and runs with \
             your rights; it shows in their chat as \"{owner_name} (owner): …\". What happens \
             there comes back here. Questions from your turns are asked here, of you only. \
             Commands stay your own. /leave to stop.",
            label = b.label,
            sid = b.sid,
            who = j.speaker,
        )
    }

    /// `/leave` from the owner's chat whose turn key is `owner_key`.
    fn leave_command(&self, owner_key: &str) -> String {
        let gone = self.joins.lock_ok().remove(owner_key);
        let _ = self
            .store
            .lock_ok()
            .meta_delete(&Self::join_store_key(owner_key));
        // A development draft briefly stored a parallel desktop record.
        // Retire it with the shared binding; it never authorizes a turn.
        if let Some(join_id) = owner_key.strip_prefix("desktop:join:") {
            let _ = self
                .store
                .lock_ok()
                .meta_delete(&format!("join:desktop:v1:{join_id}"));
        }
        match gone {
            Some(b) => {
                self.unpublish_join(&b.sid);
                tracing::info!("join: {owner_key} left {}", b.sid);
                format!(
                    "left {}; what you write here is your own session's again",
                    b.label
                )
            }
            None => "you have not joined a session from this chat".to_string(),
        }
    }

    /// The owner's plain line in a chat joined to `b.sid`: into that
    /// session as a join row, and into its chat as the owner's words.
    fn join_line(
        &self,
        msg: &InboundMessage,
        principal: &veles_core::acl::Principal,
        owner_key: &str,
        b: &JoinBinding,
    ) -> anyhow::Result<()> {
        self.live_join_target(b)?;
        let say = |text: String| {
            self.bus.send_outbound_to(
                &msg.platform,
                msg.route.as_deref(),
                reply_plain(msg, text, MessageKind::Reply),
            );
        };
        let Some((platform, chat)) = self.session_chat(&b.sid) else {
            self.joins.lock_ok().remove(owner_key);
            let _ = self
                .store
                .lock_ok()
                .meta_delete(&Self::join_store_key(owner_key));
            self.unpublish_join(&b.sid);
            say(format!(
                "{} is no longer open here, so you have left it; /join lists what is",
                b.label
            ));
            return Err(anyhow::anyhow!(
                "{} is no longer open here, so you have left it; /join lists what is",
                b.label
            ));
        };
        let text = msg.text.trim();
        let busy = self
            .session_lock_key(&b.sid)
            .is_some_and(|k| self.turn_lock(&k).try_lock().is_err());
        if let Err(e) =
            veles_core::mail::send_join(&self.store, principal, &b.owner_name, &b.sid, text)
        {
            say(format!("[veles] could not write into {}: {e:#}", b.label));
            return Err(e.context("could not enqueue the joined session's owner line"));
        }
        // Their chat sees the owner's words, unless the owner wrote them
        // there.
        if (platform.as_str(), chat.as_str()) != (msg.platform.as_str(), msg.chat_id.as_str()) {
            self.bus.send_outbound_to(
                &platform,
                None,
                OutboundMessage::plain(
                    &chat,
                    format!("{} (owner): {text}", b.owner_name),
                    MessageKind::Reply,
                ),
            );
        }
        if busy {
            say(format!(
                "(queued: {} has a turn running; yours runs after it)",
                b.label
            ));
        }
        Ok(())
    }

    /// Exactly-once gate: claim the message's dedup key in the durable
    /// ledger. Returns true when it's a duplicate (caller drops it).
    /// On a ledger error we fail OPEN (process the message) and log —
    /// dropping user input on an internal error is worse than the
    /// vanishingly rare double delivery it guards against.
    fn seen_before(&self, msg: &InboundMessage) -> bool {
        let key = dedup_key(msg);
        match self.store.lock_ok().claim_inbound(&key) {
            Ok(fresh) => !fresh,
            Err(e) => {
                tracing::error!("dedup claim failed for {key}, failing open: {e}");
                false
            }
        }
    }

    /// Human-readable gateway state for the `/status` builtin (and any
    /// surface that asks). Runs inside the gateway, so the answer is
    /// authoritative: we ARE running.
    fn status_text(&self) -> String {
        let mut lines = vec![format!(
            "veles gateway · up (pid {}, v{})",
            std::process::id(),
            env!("CARGO_PKG_VERSION")
        )];
        if let Some(u) = veles_core::lifecycle::uptime_from_records() {
            let (h, m, s) = (u / 3600, (u % 3600) / 60, u % 60);
            lines.push(if h > 0 {
                format!("uptime {h}h {m}m {s}s")
            } else {
                format!("uptime {m}m {s}s")
            });
        }
        let platforms: Vec<String> = self.bus.outbound_platforms();
        if platforms.is_empty() {
            lines.push("platforms: none connected".to_string());
        } else {
            lines.push(format!("platforms: {}", platforms.join(", ")));
        }
        let m = veles_core::repair::metrics();
        lines.push(format!(
            "args: {} exact · {} fenced · {} repaired · {} bare · {} coerced · {} refused · {} alias hits",
            m.exact, m.fenced, m.repaired, m.bare, m.coerced, m.refused, m.alias
        ));
        lines.push(format!(
            "sessions: {} · cron: {} job(s)",
            self.sessions.lock_ok().len(),
            self.store
                .lock()
                .unwrap()
                .cron_list(true)
                .map(|j| j.len())
                .unwrap_or(0)
        ));
        // Sessions currently in plan mode. `/status` is the one surface
        // every frontend already has, so naming plan mode here means no
        // frontend can be entirely silent about it — the per-frontend
        // indicators below are for visibility mid-turn, not for this.
        let planning = {
            let s = self.store.lock().unwrap();
            self.sessions
                .lock_ok()
                .keys()
                .filter(|sid| veles_core::plan::mode_for(&s, sid) == veles_core::plan::Mode::Plan)
                .count()
        };
        if planning > 0 {
            lines.push(format!(
                "plan mode: ON for {planning} session(s) — they research but change nothing"
            ));
        }
        lines.join("\n")
    }

    /// "who am I and what may I do", answered for the sender who asked
    /// — the gateway's own `/acl`, because on a messaging surface the
    /// caller is not necessarily the owner (`commands::GATEWAY_OVERRIDES`
    /// records that deliberately).
    ///
    /// It names the MASK as well as the role, because on a
    /// services-less network the mask is the identity and the thing to
    /// paste into a rule — a listing that said "unverified" was
    /// describing the normal case as a failure, and gave the owner
    /// nothing to copy.
    fn acl_self_report(
        &self,
        principal: &veles_core::acl::Principal,
        msg: &InboundMessage,
    ) -> String {
        let mut where_ = String::new();
        if let Some(n) = &msg.sender_ctx.network {
            where_ = format!(" \u{b7} on {n}");
            if let Some(c) = &msg.sender_ctx.channel {
                where_.push_str(&format!(" in {c}"));
            }
        }
        let caps = if principal.caps.is_empty() {
            "(none)".to_string()
        } else {
            principal
                .caps
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut out = format!("you are {}{where_}", principal.label());
        if let Some(mask) = &principal.mask {
            out.push_str(&format!(" \u{b7} mask {mask}"));
        }
        out.push_str(&format!(" \u{b7} caps: {caps}"));
        let acl = if self.cfg_from_file {
            veles_core::acl::live_config()
        } else {
            self.cfg.acl.clone()
        };
        let cap_tokens = principal.max_context_tokens(&acl);
        if cap_tokens > 0 {
            out.push_str(&format!(
                " \u{b7} context ceiling {}k tokens",
                cap_tokens / 1000
            ));
        }
        out
    }

    /// Per-session turn lock — keeps arrival order within one session
    /// while letting every other session run concurrently. `key` is the
    /// session key (`session_key_for`), or `session_lock_key` for a path
    /// that only holds a session id.
    fn turn_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.turn_locks.lock_ok();
        locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// A permit for one of this subject's concurrent turns, or `None`
    /// when the bound is off (`0` = unbounded, which is what owners and
    /// admins resolve to). `Err(limit)` is a refusal, carrying the limit
    /// for the message that explains it.
    ///
    /// The limit is an ARGUMENT, read live by the caller from the ACL, so
    /// admission is always against the number that is in force now — and
    /// against the turns actually in flight, whichever limit they took
    /// theirs under.
    fn turn_permit(&self, subject: &str, limit: u32) -> Result<Option<TurnSlot>, u32> {
        if limit == 0 {
            return Ok(None); // unbounded
        }
        let mut map = self.turn_permits.lock_ok();
        let inflight = map.entry(subject.to_string()).or_insert(0);
        if *inflight >= limit as usize {
            return Err(limit);
        }
        *inflight += 1;
        Ok(Some(TurnSlot {
            subject: subject.to_string(),
            counts: Arc::clone(&self.turn_permits),
        }))
    }

    /// The `sessions` map key for a sender-in-chat. The key includes the
    /// principal id when there is one; the owner's identity collapses to
    /// the plain chat key so an owner-operated chat keeps its existing
    /// session/history.
    fn session_key_for(
        &self,
        platform: &str,
        chat_id: &str,
        sender_id: Option<&str>,
        ctx: &veles_core::acl::SenderContext,
    ) -> String {
        let principal = self.resolve(sender_id, ctx);
        if principal.role == veles_core::acl::Role::Owner {
            // An owner-operated chat keeps its existing session and
            // history at the plain chat key.
            return format!("{platform}:{chat_id}");
        }
        // Everyone else is keyed on their SUBJECT, not on `id`: with no
        // Services account `id` is None for every sender, so keying on
        // it collapsed a whole channel of strangers onto one session
        // and one history — and built that session's prompt under
        // whichever of them arrived first.
        format!("{platform}:{chat_id}:{}", principal.subject)
    }

    /// Resolve a sender against the LIVE ACL.
    ///
    /// Not `self.cfg.acl`: that is the boot snapshot, which is why an
    /// `[acl]` edit did nothing until a restart while
    /// `configedit::effect_for("acl")` claimed it was live. Runtime
    /// grants were always read fresh; now the file is too.
    ///
    /// WHICH source is decided once, at construction (`cfg_from_file`),
    /// not per message: a gateway built from a config FILE re-reads
    /// that file, and one built from an in-memory section (an embedder,
    /// a test) keeps it. Deciding per message would make the answer
    /// depend on whatever `$VELES_HOME` happened to point at when the
    /// message arrived.
    fn resolve(
        &self,
        sender_id: Option<&str>,
        ctx: &veles_core::acl::SenderContext,
    ) -> veles_core::acl::Principal {
        let acl = self.acl();
        // `[acl.strangers] <platform> = "<role>"` replaces
        // `default_role` on that connector only, for callers no rule
        // names. An explicit rule still wins in both directions.
        match acl.strangers_on(ctx.platform.as_deref()) {
            veles_core::acl::StrangerPolicy::Role(r) => {
                veles_core::acl::Principal::resolve_ctx_or(sender_id, ctx, &acl, r)
            }
            _ => veles_core::acl::Principal::resolve_ctx(sender_id, ctx, &acl),
        }
    }

    /// The ACL this gateway judges by: LIVE when it booted from a config
    /// file, so a ban or a door policy lands on the next message rather
    /// than the next restart; the boot snapshot when an embedder or a
    /// test handed us a config in memory and there is no file to be live
    /// about.
    fn acl(&self) -> veles_core::acl::AclConfig {
        if self.cfg_from_file {
            veles_core::acl::live_config()
        } else {
            self.cfg.acl.clone()
        }
    }

    /// Is this sender someone the connector ignores entirely?
    ///
    /// `[acl.strangers] telegram = "ignore"` answers the question
    /// `default_role` never asked — *may* an unlisted sender speak here,
    /// as opposed to *what* they may do once they have. USER,
    /// 2026-09-16: *"I want to add any other people I want to speak to
    /// it explicitly and ignore everyone else."*
    ///
    /// Separate from the `Banned` drop below it on purpose: a ban is a
    /// decision about a person, and reads as one in the log. This is a
    /// decision about the door, and says so.
    fn door_shut(&self, msg: &InboundMessage) -> bool {
        let acl = self.acl();
        if !matches!(
            acl.strangers_on(msg.sender_ctx.platform.as_deref()),
            veles_core::acl::StrangerPolicy::Ignore
        ) {
            return false;
        }
        acl.names_no_rule(msg.sender_id.as_deref(), &msg.sender_ctx)
    }

    /// The config a NEW session is built on.
    ///
    /// LIVE when this gateway boots from a config file, so a `[model]`
    /// edit reaches the next session without a restart — which is what
    /// `configedit::effect_for("model")` has always promised and what
    /// `/defaults` tells the operator when it writes. The boot snapshot
    /// when it does not: an embedder or a test that handed the gateway
    /// a config in memory has no file to be live about, and reaching
    /// for one would build its sessions out of whatever config.toml
    /// happens to exist on the machine (three alias tests failed that
    /// way, on a box with a real `~/.hermes`).
    ///
    /// The file-or-memory decision is `cfg_from_file`, captured at
    /// construction — the same gate `resolve` uses for the ACL, for the
    /// same reason: deciding per message would make the answer depend
    /// on the ambient environment instead of on what this gateway was
    /// built from.
    fn session_cfg(&self) -> Arc<veles_core::config::VelesConfig> {
        if self.cfg_from_file {
            veles_core::config::live()
        } else {
            self.cfg.clone()
        }
    }

    /// Session-per-sender (S15 ACL): `platform:chat:sender` so a guest
    /// in a shared channel never shares the owner's session history,
    /// and the frozen prompt is built under THEIR capabilities.
    ///
    /// Keyed on the principal's SUBJECT since 2026-09-09, which is what
    /// makes this true on a network with no Services: `id` was None for
    /// every sender there, so "session per sender" was really "one
    /// session for all of them".
    ///
    /// The principal is also RE-RESOLVED on every message and pushed
    /// onto the existing agent when it changed, so a grant or a
    /// revocation takes effect on the next turn instead of at the next
    /// restart (`set_principal` rebuilds the frozen prompt, which is
    /// where memory and skills are gated).
    fn agent_for_sender(
        &self,
        platform: &str,
        chat_id: &str,
        sender_id: Option<&str>,
        ctx: &veles_core::acl::SenderContext,
    ) -> Result<Arc<Agent>> {
        let principal = self.resolve(sender_id, ctx);
        let key = self.session_key_for(platform, chat_id, sender_id, ctx);
        // Which sandbox this conversation belongs to, and which ones
        // this caller may name. Resolved HERE because this is the only
        // place that holds both the principal and the conversation; the
        // tool layer reads it back by session id and by subject
        // (`sandboxes::session_sandbox` / `session_reachable`).
        let store = Some(&self.store);
        let reachable = veles_core::sandboxes::reachable(&principal, ctx, store);
        let box_name = veles_core::sandboxes::resolve_for(&principal, ctx, store);
        veles_core::sandboxes::set_session_reachable(&principal.subject, reachable);
        let session_exists = self.sessions.lock_ok().contains_key(&key);
        if !session_exists {
            // A persisted join may have restored the route before this
            // participant reconnects. Reuse its original session id.
            let sid = self
                .session_ids
                .lock_ok()
                .get(&key)
                .cloned()
                .unwrap_or_else(crate::chat::new_session_id);
            veles_core::sandboxes::set_session_sandbox(&sid, &box_name);
            // Where this session speaks from, for tools that need the
            // CONVERSATION and not just the caller — the slopdrop bridge
            // sends it on so a headless interpreter can still answer
            // `[nick]` and `[names]`.
            veles_core::acl::set_session_sender(&sid, ctx.clone());
            let registry = Arc::new(tools::builtin_registry(
                self.store.clone(),
                Some(format!("{platform}:{chat_id}")),
                &sid,
            ));
            self.store.lock_ok().create_session(&sid)?;
            // Publish session-key → session id so /cancel can resolve
            // it without waiting on the locks a running turn holds.
            // Keyed on the SENDER-SCOPED key: `/cancel` is typed into a
            // chat, but it means "stop MY turn", and a shared channel
            // has one turn per sender.
            self.session_ids.lock_ok().insert(key.clone(), sid.clone());
            // The reverse map for wake turns: a completion event names
            // the SESSION, and the waker needs the chat it belongs to.
            // Written with the forward entry, so the two can never
            // disagree about who owns which.
            self.meta
                .lock_ok()
                .insert(sid.clone(), (platform.to_string(), chat_id.to_string()));
            let display_key = if key.contains(':') && key.matches(':').count() >= 2 {
                // platform:chat:sender — show sender so the desktop
                // sidebar distinguishes guest sessions from the owner's.
                key.clone()
            } else {
                format!("{platform}:{chat_id}")
            };
            self.store.lock_ok().set_title(&sid, &display_key)?;
            let agent = Agent::new(
                // LIVE, not `self.cfg` — see `session_cfg`. This is the
                // line that made "applies to new sessions" a promise the
                // gateway could not keep: every session was born on the
                // boot snapshot, so a `/defaults model` edit did nothing
                // until the process was restarted.
                self.session_cfg(),
                self.hooks.clone(),
                registry,
                self.store.clone(),
                sid,
            )?
            .with_surface(platform)
            // Non-owner principals get their own prompt scope (memory/
            // skills gated by caps). Owner keeps the owner prompt.
            .with_principal(principal.clone())
            // Approvals and `clarify` ask in-chat: the adapter renders the
            // question (buttons on telegram, numbered options on IRC) and
            // resolves it through `veles_core::ask` — never back through the
            // inbound queue, which is parked on this very turn.
            .with_asker(Arc::new(veles_core::ask::ChatAsker {
                bus: self.bus.clone(),
                platform: platform.to_string(),
                chat_id: chat_id.to_string(),
                // The session's SUBJECT — asks opened by this session
                // can only be answered by this subject (or an owner).
                // It used to be `principal.id`, which is None wherever
                // there is no Services account: every ask was then
                // owned by nobody, and `resolve_for_chat` read that as
                // "anybody may answer".
                sender_id: Some(principal.subject.clone()),
                // A CHANNEL on IRC (or anywhere) has an audience beyond
                // the session's owner — bystander asks born on other
                // surfaces are not relayed into it. DM-shaped chats
                // (telegram chats, IRC queries) are single-user: relay
                // on. The `#` prefix is the shape every IRC adapter
                // reports for channels; anything else is a DM/DM-like
                // chat id.
                shared_audience: platform == "irc" && chat_id.starts_with('#'),
            }));
            let agent = Arc::new(agent);
            // `[[model.binds]]` — the route a NEW session starts on.
            //
            // HERE, and not per turn: "the default for new sessions" is
            // what it means, and it is the only place it does not fight
            // `/model`. Applied to a session that has just been created,
            // so there is nothing to overwrite; `apply_model_bind` still
            // checks, because the other caller (`rebind_session`) forks
            // and a fork carries the previous session's overrides
            // forward.
            self.apply_model_bind(&agent, &principal, ctx);
            // or_insert: a racing creator (only possible outside the chat
            // lock) must not get a second agent for the same chat.
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .entry(key)
                .or_insert(agent)
                .clone())
        } else {
            let agent = self
                .sessions
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .expect("session vanished between check and get");
            // The ACL may have changed since this session was built —
            // a grant, a revocation, a config edit. Re-resolving per
            // message is what makes those immediate; without it only
            // `banned` was, because that is checked on the inbound
            // path rather than on the agent.
            // A `/sandbox use` override lives on the session, so this
            // does NOT reassert the resolved box on every message —
            // only when the session has none yet (a gateway restarted
            // under a live session map cannot happen, but a session
            // built before this landed can).
            if veles_core::sandboxes::session_sandbox(&agent.session_id).is_none() {
                veles_core::sandboxes::set_session_sandbox(&agent.session_id, &box_name);
            }
            // Re-published every message, unlike the sandbox: a nick
            // change rewrites the mask, and a stale mask is what `[nick]`
            // would answer with.
            veles_core::acl::set_session_sender(&agent.session_id, ctx.clone());
            // The session's OWN principal: during the owner's line in a
            // joined session the turn's is the owner's, and re-resolving
            // that away mid-turn is exactly what must not happen.
            let current = agent.standing_principal();
            if current.as_deref() != Some(&principal) {
                tracing::info!(
                    "acl: {} re-resolved to {} — rebuilding the session prompt",
                    principal.subject,
                    principal.role.as_str()
                );
                agent.set_principal(principal);
            }
            Ok(agent)
        }
    }

    /// `/sudo <command>` from a caller who holds a role by ticket
    /// (`veles_core::ticketed`): the principal a ticket for exactly this
    /// command grants, or the words the sender gets instead. The ticket
    /// names the sender's own session, so the owner sees whose it is.
    async fn sudo(
        &self,
        msg: &InboundMessage,
        principal: &veles_core::acl::Principal,
        inner: &str,
    ) -> std::result::Result<veles_core::acl::Principal, String> {
        if principal.ticket_role.is_none() {
            return Err(
                "`/sudo` runs one command at a role you hold by ticket, and you hold none here."
                    .into(),
            );
        }
        if let Some(why) = veles_core::ticketed::sudo_refusal(inner) {
            return Err(why);
        }
        let agent = self
            .agent_for_sender(
                &msg.platform,
                &msg.chat_id,
                msg.sender_id.as_deref(),
                &msg.sender_ctx,
            )
            .map_err(|e| format!("`/sudo`: no session for this chat: {e:#}"))?;
        let cfg = veles_core::config::live();
        veles_core::ticketed::obtain(
            &self.store,
            &cfg,
            principal,
            &agent.session_id,
            veles_core::ticketed::SUDO_TOOL,
            &veles_core::ticketed::sudo_args(inner),
            "a command above the standing level",
        )
        .await
        .map_err(|e| format!("`/sudo` did not run `{inner}`: {e:#}"))
    }

    /// The gateway's half of a wake turn: admit like `handle`'s front
    /// (rate ledger + concurrency permit, under the per-chat lock), run
    /// `run_background_wake_turn`, deliver the reply to the chat with
    /// NO route footer (a wake turn was not prompted; the footer's
    /// "which model answered" is noise on a message nobody asked for),
    /// and drain the parked queue exactly like any turn end.
    pub async fn gateway_wake_one(
        &self,
        bus: &GatewayBus,
        event: &veles_core::session_wake::WakeEvent,
    ) -> anyhow::Result<()> {
        let Some((platform, chat_id)) = self.session_chat(&event.session_id) else {
            anyhow::bail!("no chat recorded for session {}", event.session_id);
        };
        // The claim BEFORE any billable admission: two wake paths exist
        // in one process, and the claim is the eat; the drain inside
        // the turn is the fire.
        let mut _claim = match veles_core::delegate::try_claim_wake(&event.session_id) {
            Some(c) => c,
            None => return Ok(()), // another waker owns it
        };
        // A turn that started between the event and now delivers the
        // completion at its boundary — this wake must stand down, not
        // race it. The store is the cross-process truth, not this
        // process's live map.
        let live = self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .runs_live(30)
            .unwrap_or_default()
            .iter()
            .any(|r| r.session_id == event.session_id);
        if live || !veles_core::delegate::has_background_events(&event.session_id) {
            return Ok(());
        }
        let speaker = self
            .session_lock_key(&event.session_id)
            .unwrap_or_else(|| format!("{platform}:{chat_id}"));
        let lock = self.turn_lock(&speaker);
        // RunRegistration clears the store before the outer surface
        // releases this lock. A post-turn signal waits through that
        // short handoff, then re-checks instead of billing a stale wake.
        let held = if event.kind == "background-wake-pending" {
            lock.lock().await
        } else {
            match lock.try_lock() {
                Ok(g) => g,
                Err(_) => return Ok(()), // a turn is claiming it; its drain delivers
            }
        };
        // A user turn may have won the lock while this wake waited;
        // its own drains then own the completion.
        let live = self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .runs_live(30)
            .unwrap_or_default()
            .iter()
            .any(|r| r.session_id == event.session_id);
        if live || !veles_core::delegate::has_background_events(&event.session_id) {
            return Ok(());
        }
        let agent = match self
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .find(|a| a.session_id == event.session_id)
            .cloned()
        {
            Some(a) => a,
            None => anyhow::bail!("session {} not held", event.session_id),
        };
        // The mute gate, BEFORE any claim the bill would ride on: a
        // session with `agent.injections.background` off drains its
        // completions into suppressed notes, never into the model's
        // context — a wake turn for one would be a provider bill for
        // nothing the model was allowed to read. Its completions stay
        // queued for the next real turn, exactly as today.
        if !agent.cfg.agent.injections.background {
            return Ok(());
        }
        // The two brakes, exactly as `handle` applies them for every
        // message: the wake turn is a user-visible provider bill, so a
        // ceiling that makes it free would be the hole the brake exists
        // to close, and a turn cheaper than another message looks like
        // a bug.
        let acl_now = veles_core::acl::with_runtime_limits(
            if self.cfg_from_file {
                veles_core::acl::live_config()
            } else {
                self.cfg.acl.clone()
            },
            &self.store,
        );
        let principal = match agent.principal() {
            Some(p) => (*p).clone(),
            None => veles_core::acl::Principal::owner(),
        };
        let rate: u64 = acl_now.limits.turns_per_hour_for_subject(
            principal.role,
            &principal.subject,
            &principal.budget_key(),
        );
        if self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .claim_rate(
                veles_core::store::Store::TURN_RATE_PREFIX,
                &principal.budget_key(),
                rate,
                veles_core::agent::TURN_RATE_WINDOW_SECS,
            )
            .ok()
            .flatten()
            .is_some()
        {
            return Ok(()); // budget spent: silent, the drain covers it at the next prompt
        }
        let limit = acl_now.limits.concurrent_turns_for_role(principal.role);
        let _permit = match self.turn_permit(&principal.subject, limit) {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };
        // Only an admitted wake may ask for a follow-up on drop. A
        // muted/budget-refused/busy stand-down above leaves its events
        // parked for the next real turn, never a free retry loop.
        _claim.retry_pending_on_drop();

        let mut history = {
            agent
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .load_history(&agent.session_id)
                .unwrap_or_default()
        };
        let res = agent
            .run_background_wake_turn(&mut history, None, |_ev| {})
            .await;

        // A wake turn speaks for its session like any other, so a room
        // that last heard from someone else hears who this one is for.
        let sender = veles_core::acl::session_sender(&event.session_id);
        let out_tx = self.with_tees(
            bus.outbound_channel(&platform, None).map(|tx| {
                self.voice_on(
                    tx,
                    &platform,
                    &chat_id,
                    sender.as_ref().is_some_and(|c| c.direct),
                    &speaker,
                    sender.as_ref().and_then(|c| address_name("", c)),
                )
            }),
            &event.session_id,
        );
        if let Some(tx) = &out_tx {
            match res {
                Ok(tr) if !tr.reply.trim().is_empty() => {
                    // The media gate applies: a wake turn is outbound
                    // text like any other.
                    let (text, _) =
                        veles_core::media::enforce_outbound(&tr.reply, Some(&principal));
                    let _ = tx.send(OutboundMessage {
                        chat_id: chat_id.clone(),
                        reply_to: None,
                        text,
                        reasoning: None,
                        // Deliberately NO provider/model footer: this
                        // is a message nobody asked for, and the footer
                        // is answer-decoration, not provenance.
                        provider: String::new(),
                        model: String::new(),
                        kind: MessageKind::Reply,
                        ask: None,
                        meta: Default::default(),
                    });
                }
                Ok(_) => {} // empty reply: nothing to deliver, transcript has it
                Err(e) => {
                    let _ = tx.send(OutboundMessage::plain(
                        &chat_id,
                        format!("[veles] background wake failed: {e:#}"),
                        MessageKind::Reply,
                    ));
                }
            }
        }

        // The wake turn's boundary drains the parked queue, exactly
        // like any turn end — a message parked while the wake ran runs
        // next, and running it HERE (under the chat lock) instead of
        // through the inbound path avoids a dedup claim on text that
        // is not a wire message.
        if let Some(item) = self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue_take_next(&agent.session_id)
            .map_err(|e| {
                tracing::warn!(
                    "queue: taking {}'s next row failed: {e:#}",
                    agent.session_id
                )
            })
            .ok()
            .flatten()
        {
            // Release the row's `running` state (we took it), then push
            // it back through `handle` so it gets a turn, dedup,
            // filters and all. Drop the chat lock FIRST: `handle`
            // re-acquires it, and re-injecting while holding it would
            // re-park the message behind ourselves.
            let _ = self
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .queue_release(&agent.session_id, item.seq);
            drop(held);
            bus.send_inbound(self.parked_inbound(
                &agent.session_id,
                platform.clone(),
                chat_id.clone(),
                &item,
            ));
        }
        Ok(())
    }

    /// The inbound a parked queue row is handed back to the pipeline as.
    ///
    /// It speaks as the SESSION'S OWN SENDER — the identity its agent was
    /// built for, and the context that sender last spoke from — so
    /// `handle` resolves the same principal and the same session key that
    /// the main turn loop's drain (which runs a parked row on the very
    /// agent) would have used. It used to send an empty sender, which a
    /// configured ACL resolves as a stranger: keyed to a different,
    /// anonymous session, or refused at the door. Session mail needs the
    /// row to land in the session it was addressed to; so did every
    /// parked message a cron kick or a wake drain handed back.
    ///
    /// The `dedup_id`, not a faked `msg_id`: the parked message's real id
    /// was already claimed in the inbound dedup ledger before it parked,
    /// so re-sending the text bare would fall onto the content-hash
    /// fallback and the reply would be eaten as a duplicate. This id is
    /// unique by construction, and `dedup_key` prefers it over `msg_id`.
    ///
    /// `msg_id` therefore stays the REAL platform id the row recorded, so
    /// a threaded adapter answers under the message that asked. Faking it
    /// here was the whole bug: atproto's `deliver_reply` resolves
    /// `reply_to` through `parse_at_uri`, a `queued:` string is not an
    /// at-uri, and a reply that cannot thread is refused rather than
    /// dumped top-level on a public feed — so the answer was silently
    /// dropped, and the only trace was the sender's mail row.
    fn parked_inbound(
        &self,
        session_id: &str,
        platform: String,
        chat_id: String,
        item: &veles_core::store::QueueItem,
    ) -> InboundMessage {
        let principal = self
            .sessions
            .lock_ok()
            .values()
            .find(|a| a.session_id == session_id)
            .and_then(|a| a.standing_principal());
        let sender_ctx = veles_core::acl::session_sender(session_id)
            .map(|c| (*c).clone())
            .unwrap_or_default();
        InboundMessage {
            platform,
            // The adapter instance the question came in on: two IRC
            // networks are two audiences, and the answer must leave the
            // way the question arrived.
            route: item.route.clone(),
            chat_id,
            msg_id: item.reply_to.clone(),
            dedup_id: Some(format!(
                "queued:{}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                item.seq
            )),
            sender: String::new(),
            sender_id: principal.and_then(|p| p.id.clone()),
            sender_ctx,
            text: item.body.clone(),
            context: None,
        }
    }

    /// Hand parked queue rows back to the inbound pipeline. The cron
    /// wakeup calls this after a foreign turn ended under our chat
    /// lock: a message that parked during it has no boundary left to
    /// wait for (the boundary was the cron turn's, and its drainer is
    /// not ours). One re-injection is enough — the re-injected message
    /// runs the full `handle` path, whose OWN boundary drain takes the
    /// next row, and so on. The synthetic `queued:` msg_id and the
    /// release-before-reinject dance are `gateway_wake_one`'s exactly:
    /// the parked row's real msg_id was claimed in the dedup ledger
    /// before it parked, and re-injecting while a taker holds the row
    /// would leave a `running` corpse the crash-recovery demotion
    /// re-runs forever.
    pub fn kick_parked_queue(&self, session_id: &str) {
        let Some((platform, chat_id)) = self.session_chat(session_id) else {
            return;
        };
        let item = match self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue_take_next(session_id)
            .map_err(|e| tracing::warn!("queue: taking {}'s next row failed: {e:#}", session_id))
            .ok()
            .flatten()
        {
            Some(i) => i,
            None => return,
        };
        let _ = self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue_release(session_id, item.seq);
        self.bus
            .send_inbound(self.parked_inbound(session_id, platform, chat_id, &item));
    }

    /// Swap this chat's `Arc<Agent>` for a freshly built agent bound to
    /// `new_sid`. This is `/new` and `/resume` on the chat surfaces: the
    /// shared dispatcher holds only a `&Agent`, and a session switch
    /// swaps the owned `Arc` — so it has to live here, where the
    /// `sessions` map is reachable.
    async fn rebind_session(
        &self,
        platform: &str,
        chat_id: &str,
        sender_id: Option<&str>,
        ctx: &veles_core::acl::SenderContext,
        new_sid: &str,
    ) -> Result<Arc<Agent>> {
        let key = self.session_key_for(platform, chat_id, sender_id, ctx);
        let old = self
            .sessions
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no session for this chat"))?;
        let fresh = Arc::new(crate::fork(
            &old,
            // LIVE, for the reason `session_cfg` gives: `/new` mints a
            // session, and `effect_for("model")` classifies `[model]` as
            // NewSessions. Forking on `old.cfg` made that promise false
            // for the one command an operator uses to pick up a changed
            // default. The session's own `/model` choice still wins —
            // `fork` carries the overrides forward underneath this.
            self.session_cfg(),
            new_sid,
            Some(format!("{platform}:{chat_id}")),
        )?);
        self.session_ids
            .lock_ok()
            .insert(key.clone(), new_sid.to_string());
        self.sessions.lock().unwrap().insert(key, fresh.clone());
        // `[[model.binds]]` is deliberately NOT reapplied here, and the
        // omission is a decision rather than an oversight.
        //
        // `fork` carries the previous session's overrides forward, so a
        // `/new` after a `/model` keeps the route the person chose —
        // which is the existing behaviour and the right one. Applying a
        // bind here would either be skipped by that inherited override
        // (doing nothing, but emitting a note claiming otherwise) or
        // stamp over an explicit choice.
        //
        // The chat-alias path sets its own route immediately after this
        // returns, so a bind applied here would be overwritten anyway
        // and the note would name a route the session is not on.
        //
        // A bind's job is the FIRST session a person gets, which is
        // `agent_for_sender`'s creation path.
        Ok(fresh)
    }

    /// Point a freshly built session at whatever `[[model.binds]]` says,
    /// and say so in the transcript.
    ///
    /// Three rules, each of which was easy to get wrong:
    ///
    /// * **It fills, it does not override.** A session that already has
    ///   a model override keeps it. Otherwise `/model` would survive
    ///   only until the next fork, which is the opposite of what a
    ///   session-scoped override means.
    /// * **It runs on the FRESH agent, after any fork.**
    ///   `frontends::fork` carries the previous session's overrides
    ///   forward, so a bind applied to the old handle changes nothing.
    /// * **It announces itself.** A route nobody typed, that changes
    ///   what the turn costs, is exactly the implicit action the notes
    ///   doctrine exists to surface.
    fn apply_model_bind(
        &self,
        agent: &Arc<Agent>,
        principal: &veles_core::acl::Principal,
        ctx: &veles_core::acl::SenderContext,
    ) {
        if agent.overrides.read_ok().model.is_some() {
            return; // an explicit choice wins, and keeps winning
        }
        let Some(b) =
            veles_core::model_binds::resolve(&agent.cfg, principal, ctx, Some(&self.store))
        else {
            return;
        };
        let Some(arg) = veles_core::model_binds::model_arg(&b) else {
            return;
        };
        match agent.set_model(&arg) {
            Ok(resolved) => {
                if let Some(r) = b.reasoning.as_deref() {
                    agent.set_reasoning(r);
                }
                agent.queue_note(veles_core::model_binds::note_for(&b, &resolved));
            }
            Err(e) => {
                // A bind naming a route that will not resolve is a
                // config error, and the session carries on at the
                // default rather than being unusable — but it says so,
                // because "why is this on the wrong model" is
                // unanswerable otherwise.
                tracing::warn!("model bind {arg} did not resolve: {e:#}");
                agent.queue_note(
                    veles_core::notes::EngineNote::new(
                        veles_core::notes::NoteKind::Route,
                        format!("a [[model.binds]] rule names {arg}, which did not resolve"),
                    )
                    .with_detail(format!(
                        "{e:#}\nThis session is on the [model] default instead. \
                         `/defaults binds` lists the rules."
                    )),
                );
            }
        }
    }

    /// S9: `/pair` — redeem a pairing code (any verified sender) or
    /// manage codes/grants (owner only). Returns the reply text.
    fn handle_pair(
        &self,
        rest: &str,
        principal: &veles_core::acl::Principal,
        msg: &InboundMessage,
    ) -> String {
        let Some(identity) = msg.sender_id.as_deref() else {
            return "pairing needs a verified platform identity; this platform can't verify senders".into();
        };
        let mut parts = rest.split_whitespace();
        let sub = parts.next().unwrap_or("");

        match sub {
            // Redeem: /pair K7Q2-M9X4
            code if !code.is_empty() && !code.starts_with('/') && veles_core::pairing::valid_shape(code) =>
            {
                // Already have a grant? Don't consume a second code.
                if let Some(role) =
                    veles_core::acl::runtime_pairing_store()
                        .as_ref()
                        .and_then(|s| veles_core::pairing::runtime_role(s, identity))
                {
                    return format!(
                        "you already have a runtime grant ({}) — ask the owner to revoke it first",
                        role.as_str()
                    );
                }
                match veles_core::pairing::redeem(&self.store, code, identity) {
                    Ok(role) => format!(
                        "paired! identity {identity} now has the '{}' role. /status shows your capabilities.",
                        role
                    ),
                    Err(e) => format!("pairing failed: {e}"),
                }
            }
            // Owner management.
            "new" => {
                if !principal.has(veles_core::acl::Cap::All) {
                    return "[acl] only the owner can create pairing codes.".into();
                }
                let mut role = "user";
                let mut ttl_hours: Option<u64> = None;
                let mut args = parts.collect::<Vec<_>>().into_iter();
                while let Some(a) = args.next() {
                    match a {
                        "--role" => {
                            if let Some(r) = args.next() {
                                role = r;
                            }
                        }
                        "--ttl-hours" => {
                            if let Some(h) = args.next().and_then(|v| v.parse::<u64>().ok()) {
                                ttl_hours = Some(h * 3600);
                            }
                        }
                        _ => {}
                    }
                }
                match veles_core::pairing::create(&self.store, role, ttl_hours) {
                    Ok((code, exp)) => {
                        let hours_left = exp.saturating_sub(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        ) / 3600;
                        format!(
                            "pairing code: {code}\nrole: {role} · expires in {hours_left}h\nthe recipient sends: /pair {code}"
                        )
                    }
                    Err(e) => format!("error: {e}"),
                }
            }
            "list" => {
                if !principal.has(veles_core::acl::Cap::All) {
                    return "[acl] only the owner can list grants.".into();
                }
                let grants = veles_core::pairing::list_grants(&self.store);
                if grants.is_empty() {
                    return "no live pairing grants.".into();
                }
                let mut out = String::from("live pairing grants:\n");
                for g in grants {
                    out.push_str(&format!(
                        "  {} → {} (granted {})\n",
                        g.identity,
                        g.role,
                        chrono::DateTime::<chrono::Local>::from(
                            std::time::UNIX_EPOCH
                                + std::time::Duration::from_secs(g.granted_at)
                        )
                        .format("%Y-%m-%d %H:%M")
                    ));
                }
                out
            }
            "revoke" => {
                if !principal.has(veles_core::acl::Cap::All) {
                    return "[acl] only the owner can revoke grants.".into();
                }
                let Some(target) = parts.next() else {
                    return "usage: /pair revoke <identity>".into();
                };
                if veles_core::pairing::revoke(&self.store, target) {
                    format!("revoked: {target}")
                } else {
                    format!("no grant found for {target}")
                }
            }
            "" => {
                "usage: /pair <CODE> to redeem · owner: /pair new [--role trusted] [--ttl-hours N] | list | revoke <identity>".into()
            }
            other => format!("unknown /pair subcommand '{other}'"),
        }
    }

    /// Every inbound message, through the inbound-routing slot (R7,
    /// `veles_core::layers`): a module's layer sees it before the ACL,
    /// the commands and the turn, and may drop, rewrite or pass it.
    /// Nothing installed is the direct call.
    async fn handle(&self, msg: InboundMessage) {
        // The `inbound.route` override point (PLAN-extension-api.md E3),
        // when a plugin overrides it: route the message (its text maybe
        // changed) or drop it, before the module layers below.
        let msg = if veles_core::services::overridden(veles_core::points::INBOUND) {
            match inbound_point(msg).await {
                Some(m) => m,
                None => return,
            }
        } else {
            msg
        };
        let layers = veles_core::layers::inbound_layers();
        if layers.is_empty() {
            return self.handle_default(msg).await;
        }
        veles_core::layers::run_inbound_layers(&layers, msg, inbound_base(self)).await
    }

    /// Core's own routing: the bottom of the inbound chain.
    async fn handle_default(&self, msg: InboundMessage) {
        self.handle_as(msg, None).await
    }

    /// The routing, for a sender resolved here (`raised` None) or for one
    /// command a `/sudo` ticket raised (`raised` = the principal that
    /// ticket granted, `veles_core::ticketed`). Boxed: `/sudo` re-enters
    /// it with the command it raised.
    fn handle_as(
        &self,
        msg: InboundMessage,
        raised: Option<veles_core::acl::Principal>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.handle_as_inner(msg, raised))
    }

    async fn handle_as_inner(
        &self,
        msg: InboundMessage,
        raised: Option<veles_core::acl::Principal>,
    ) {
        let platform = msg.platform.clone();
        // The adapter INSTANCE this came in on used to be cloned here
        // beside `platform`, so a reply built after `msg` was consumed
        // still went back out the connection that carried the question.
        // The turn loop now holds `cur_msg` (its own clone, per drained
        // row) for exactly that reason and more: a row that parked on one
        // network must not have its answer sent out the other. Everything
        // past this point reads `cur_msg`, so nothing reads `msg.route`
        // after the loop starts.
        // ── ACL: resolve the sender BEFORE anything else ──
        // Against the LIVE section, so a ban or a grant lands on the
        // next message rather than the next restart.
        // The door, before the person: a connector set to ignore
        // strangers never resolves one into a turn, a reply, or a
        // billed token. One INFO line, because a bot that silently
        // discards mail is indistinguishable from one nobody wrote to.
        if self.door_shut(&msg) {
            tracing::info!(
                "acl: ignored {} message from {} — [acl.strangers] admits only identities an \
                 [acl] rule names on this connector",
                msg.platform,
                msg.sender_id.as_deref().unwrap_or("an unidentified sender"),
            );
            return;
        }
        let principal = match raised {
            Some(p) => p,
            None => self.resolve(msg.sender_id.as_deref(), &msg.sender_ctx),
        };
        if principal.role == veles_core::acl::Role::Banned {
            tracing::warn!(
                "acl: dropped inbound from banned identity {}",
                principal.subject
            );
            return;
        }
        // The session this sender-in-chat resolves to: the turn lock, the
        // progress window and the room's addressing are all keyed on it,
        // so two people in one channel run side by side.
        let turn_key = self.session_key_for(
            &msg.platform,
            &msg.chat_id,
            msg.sender_id.as_deref(),
            &msg.sender_ctx,
        );

        // ── a PUBLIC feed: a command's answer is a public post ──
        // Before every command block below (`/queue`, the builtins,
        // `/pair`, the shared dispatcher), so none of them can answer a
        // mutual on a Bluesky feed with the pool's internals.
        if let Some(refusal) = public_feed_refusal(&msg.platform, &msg.text, &principal) {
            self.bus.send_outbound_to(
                &msg.platform,
                msg.route.as_deref(),
                reply_plain(&msg, refusal, MessageKind::Reply),
            );
            return;
        }

        // ── lockdown: this whole function IS the connector path ──
        //
        // Everything that reaches `handle_message` arrived over a
        // connector (IRC, Telegram, A2A); the local frontends call the
        // agent directly and never come through here. So one gate,
        // here, before the chat lock — where it cannot be starved by a
        // turn that is already running — refuses turns, commands, tool
        // calls and ask answers alike, whatever the sender's role. An
        // owner over IRC is still a connector.
        //
        // `/lockdown` itself is let through on purpose: a sender who
        // has just been refused should be able to ask what refused
        // them, `off` from here answers with the reason it will not
        // work, and `on` while already on needs `cmd-admin` and can
        // change nothing but the attribution. Silence on that one name
        // would make the feature unexplainable from the place people
        // meet it.
        if !msg.text.trim_start().starts_with("/lockdown") {
            if let Some(l) = veles_core::lockdown::current(Some(&self.store)) {
                tracing::info!(
                    "lockdown: refused inbound from {} on {}",
                    principal.subject,
                    msg.platform
                );
                // One notice per sender, then silence: answering every
                // line would turn a locked-down bot into the loudest
                // thing in the channel.
                if veles_core::lockdown::notice_once(&principal.subject) {
                    self.bus.send_outbound_to(
                        &msg.platform,
                        msg.route.as_deref(),
                        reply_plain(&msg, veles_core::lockdown::refusal(&l), MessageKind::Reply),
                    );
                }
                return;
            }
        }

        // Exactly-once BEFORE the per-chat lock: a redelivered update
        // must neither answer twice nor spawn a second queue row. Moved
        // up from below the lock so the parking path also skips
        // duplicates — otherwise a redelivery would park and then run.
        if self.seen_before(&msg) {
            tracing::info!(
                "duplicate inbound dropped ({}:{})",
                msg.platform,
                msg.msg_id.as_deref().unwrap_or("content-hash")
            );
            return;
        }

        // ── /sudo: ONE command at a role held by ticket ──
        // (`ticket_role` in `[acl]`, `veles_core::ticketed`.) After the
        // duplicate check, because both Bluesky lanes deliver a mention
        // and a second delivery must not put a second request in front
        // of the owner; before the chat lock, because the owner may take
        // minutes. The raised command then runs through this same
        // routing with the principal the ticket granted — that one
        // command only: the session's agent keeps its standing principal.
        if let Some(inner) = veles_core::ticketed::sudo_target(&msg.text) {
            match self.sudo(&msg, &principal, inner).await {
                Ok(raised) => {
                    let mut inner_msg = msg.clone();
                    inner_msg.text = inner.to_string();
                    inner_msg.msg_id = msg.msg_id.as_ref().map(|id| format!("{id}#sudo"));
                    return self.handle_as(inner_msg, Some(raised)).await;
                }
                Err(text) => {
                    self.bus.send_outbound_to(
                        &msg.platform,
                        msg.route.as_deref(),
                        reply_plain(&msg, text, MessageKind::Reply),
                    );
                    return;
                }
            }
        }

        // /cancel BEFORE the per-chat lock, and this ordering is the
        // whole feature.
        //
        // The lock below serializes a chat's messages so their turns
        // cannot interleave. A cancel taken after it would queue behind
        // the very turn it exists to stop and arrive once that turn had
        // finished on its own — a cancel that always reports success and
        // never cancels anything, which is the failure this change was
        // made to remove from the A2A server. So it is answered here,
        // from the session-id map, touching neither the chat lock nor
        // the agent lock (the running turn holds both).
        // /detach rides the SAME fast lane: it arms a flag the running
        // turn's foreground terminal command polls, so a detach taken
        // after the lock would queue behind the turn that is blocking
        // on the very command it wants to move. Identical shape, one
        // word different: request_detach instead of cancel_turn, and an
        // honest "nothing to detach" when the turn is not inside a
        // foreground terminal call right now.
        {
            let t = msg.text.trim();
            if t == "/detach" {
                if !principal.has(veles_core::acl::Cap::CmdSession) {
                    self.bus.send_outbound_to(
                        &msg.platform,
                        msg.route.as_deref(),
                        reply_plain(
                            &msg,
                            format!(
                                "[acl] `detach` requires a higher access level ({}) — denied for {}.",
                                veles_core::acl::Cap::CmdSession.as_str(),
                                principal.label()
                            ),
                            MessageKind::Reply,
                        ),
                    );
                    return;
                }
                let own_key = self.session_key_for(
                    &msg.platform,
                    &msg.chat_id,
                    msg.sender_id.as_deref(),
                    &msg.sender_ctx,
                );
                let sid = self.session_ids.lock_ok().get(&own_key).cloned();
                let reply = match sid {
                    Some(sid) if veles_core::agent::request_detach(&sid) => {
                        "detaching — the command keeps running as a background shell".to_string()
                    }
                    Some(sid) if veles_core::agent::turn_is_live(&sid) => {
                        "nothing to detach: the running turn is not inside a foreground terminal command right now".to_string()
                    }
                    _ => "nothing running in this chat".to_string(),
                };
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            if t == "/cancel" || t == "/stop" {
                // Same tier the builtin table would apply
                // (`cancel` ⇒ CmdSession). The fast path used to skip
                // it: anything past the Banned check could cancel the
                // chat's turn — a Guest, in a shared channel, stopping
                // someone else's work. The principal is already
                // resolved above; the check costs one set lookup and
                // must not be bypassed by ordering again.
                if !principal.has(veles_core::acl::Cap::CmdSession) {
                    self.bus.send_outbound_to(
                        &msg.platform,
                        msg.route.as_deref(),
                        reply_plain(
                            &msg,
                            format!(
                                "[acl] `cancel` requires a higher access level ({}) — denied for {}.",
                                veles_core::acl::Cap::CmdSession.as_str(),
                                principal.label()
                            ),
                            MessageKind::Reply,
                        ),
                    );
                    return;
                }
                // The CALLER's session, not "the chat's": in a shared
                // channel the chat has one running turn per sender.
                let own_key = self.session_key_for(
                    &msg.platform,
                    &msg.chat_id,
                    msg.sender_id.as_deref(),
                    &msg.sender_ctx,
                );
                let sid = self.session_ids.lock_ok().get(&own_key).cloned();
                let reply = match sid {
                    Some(sid) if veles_core::agent::cancel_turn(&sid) => {
                        "cancelling the running turn…".to_string()
                    }
                    _ => "nothing running in this chat".to_string(),
                };
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
        }

        // /queue and /steer also answer BEFORE the per-chat lock, for the
        // same reason /cancel does: both are reads or appends against the
        // RUNNING session, not turns themselves, so a command typed while
        // a turn runs must answer before it ends. /queue lists, edits and
        // cancels the SAME store rows the desktop and TUI touch (one
        // queue, visible everywhere); /steer writes the running turn's
        // pending_steer — resolved through the BUSY session's agent, not a
        // fresh instance, so the turn that is actually running reads it at
        // its next tool round. Both carry CmdSession, the tier the shared
        // dispatcher applied to them and the same gateway-level check that
        // rides every slash command.
        {
            let trimmed = msg.text.trim_start();
            if let Some(cmd) = trimmed.strip_prefix('/') {
                let mut words = cmd.split_whitespace();
                let head = words.next().unwrap_or("");
                match head {
                    "join" | "leave" => {
                        // The owner's alone: a joined chat's lines run in
                        // someone else's session with the owner's rights.
                        if principal.role != veles_core::acl::Role::Owner
                            || !principal.has(veles_core::acl::Cap::All)
                        {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!(
                                        "[acl] `{head}` is the owner's — denied for {}.",
                                        principal.label()
                                    ),
                                    MessageKind::Reply,
                                ),
                            );
                            return;
                        }
                        let rest = words.collect::<Vec<_>>().join(" ");
                        let reply = if head == "join" {
                            self.join_command(&msg, &principal, &turn_key, &rest)
                        } else {
                            self.leave_command(&turn_key)
                        };
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(&msg, reply, MessageKind::Reply),
                        );
                        return;
                    }
                    "queue" => {
                        if !principal.has(veles_core::acl::Cap::CmdSession) {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!(
                                        "[acl] `queue` requires a higher access level ({}) — denied for {}.",
                                        veles_core::acl::Cap::CmdSession.as_str(),
                                        principal.label()
                                    ),
                                    MessageKind::Reply,
                                ),
                            );
                            return;
                        }
                        let agent = match self.agent_for_sender(
                            &msg.platform,
                            &msg.chat_id,
                            msg.sender_id.as_deref(),
                            &msg.sender_ctx,
                        ) {
                            Ok(a) => a,
                            Err(e) => {
                                tracing::error!("agent_for failed: {e}");
                                return;
                            }
                        };
                        let rest = words.collect::<Vec<_>>().join(" ");
                        let reply = super::commands::queue_command(&agent, &rest);
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(&msg, reply, MessageKind::Reply),
                        );
                        return;
                    }
                    "steer" => {
                        if !principal.has(veles_core::acl::Cap::CmdSession) {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!(
                                        "[acl] `steer` requires a higher access level ({}) — denied for {}.",
                                        veles_core::acl::Cap::CmdSession.as_str(),
                                        principal.label()
                                    ),
                                    MessageKind::Reply,
                                ),
                            );
                            return;
                        }
                        let rest = words.collect::<Vec<_>>().join(" ");
                        if rest.is_empty() {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    "usage: /steer <message>\n  injects after the next tool call — redirects the agent mid-turn",
                                    MessageKind::Reply,
                                ),
                            );
                            return;
                        }
                        // ROUTED through the process-global steer slot
                        // (`ask::steer`), not this cache's agent: the
                        // running turn may belong to the DESKTOP's
                        // agent instance for the same session id, and
                        // steering our own `pending_steer` was the
                        // silent loss (200-shaped "queued" into an
                        // Option nobody reads, PENDING 2026-09-14). No
                        // live turn at all → the steer docks and the
                        // next turn start-injects it, on any surface.
                        let sid = self
                            .session_ids
                            .lock()
                            .unwrap()
                            .get(&self.session_key_for(
                                &msg.platform,
                                &msg.chat_id,
                                msg.sender_id.as_deref(),
                                &msg.sender_ctx,
                            ))
                            .cloned();
                        let Some(sid) = sid else {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    "no session for this chat yet — send a message first",
                                    MessageKind::Reply,
                                ),
                            );
                            return;
                        };
                        let note = match veles_core::ask::steer(&sid, &rest) {
                            veles_core::ask::Steered::Live => {
                                format!("steer queued: '{rest}' — will inject after next tool call")
                            }
                            // Said out loud on purpose: a steer landing
                            // in the same gap as another used to
                            // REPLACE it silently. Both arrive now, and
                            // the sender is told theirs is not the only
                            // one the turn will read.
                            veles_core::ask::Steered::LiveQueued { behind } => format!(
                                "steer queued: '{rest}' — behind {behind} already waiting; \
all of them inject after the next tool call"
                            ),
                            veles_core::ask::Steered::Parked => {
                                // Honest about what parked means: the
                                // user may believe a compaction or a
                                // stuck-looking turn is running.
                                format!(
                                    "steer parked: '{rest}' — no turn is live, it will open \
the next turn on this session"
                                )
                            }
                        };
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(&msg, note, MessageKind::Reply),
                        );
                        return;
                    }
                    _ => {}
                }
            }
        }

        // ── the owner's line in a chat joined to someone's session ──
        // Before this chat's own lock: the line is not a turn of the
        // owner's session, it is the next turn of the joined one.
        if !msg.text.trim_start().starts_with('/')
            && principal.role == veles_core::acl::Role::Owner
            && principal.has(veles_core::acl::Cap::All)
        {
            if let Some(b) = self.join_of(&turn_key) {
                if let Err(e) = self.join_line(&msg, &principal, &turn_key, &b) {
                    tracing::warn!("join input failed: {e:#}");
                }
                return;
            }
        }

        // Per-session ordering: taken before session creation so two
        // messages from the same sender-in-chat can never interleave their
        // turns (and can never both create the session). Another sender in
        // the same channel has their own session and their own lock. But it is no longer a blocking
        // wait for plain chat messages: those PARK in the shared queue
        // while a turn runs, so they are visible and cancellable to
        // /queue, and the turn boundary drains them. Slash commands still
        // serialize — the state changes they make must not interleave with
        // a running turn.
        let lock = self.turn_lock(&turn_key);
        let is_plain_chat = !msg.text.trim_start().starts_with('/');
        // A command's end drains what parked behind it (see the struct).
        let _drain_after = (!is_plain_chat).then(|| DrainAfterCommand {
            core: self,
            turn_key: turn_key.clone(),
        });
        let _turn_guard = if is_plain_chat {
            match lock.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    // A turn is running (ours or the desktop's/TUI's). Park
                    // this message in the shared queue. The transforms that
                    // run before ANY turn — the MEDIA gate, the Tcl
                    // hook, and the rust filters — still apply, so a
                    // parked message is the text the model will eventually
                    // read, not the raw inbound line.
                    let (defanged, withheld) =
                        veles_core::media::enforce_inbound(&msg.text, Some(&principal));
                    if !withheld.is_empty() {
                        tracing::warn!(
                            "[acl] withheld {} inbound MEDIA marker(s) from {} ({}): {}",
                            withheld.len(),
                            principal.subject,
                            principal.role.as_str(),
                            withheld.join(", ")
                        );
                    }
                    let mut text = defanged;
                    if let Some(r) = self.hooks_fire_gateway(&msg).await {
                        match r {
                            GatewayDecision::Skip => return,
                            GatewayDecision::Rewrite(t) => text = t,
                            GatewayDecision::Allow => {}
                        }
                    }
                    let filter_decision = if veles_core::rustplugins::has_gateway_filter() {
                        let (p, c, t) = (msg.platform.clone(), msg.chat_id.clone(), text.clone());
                        veles_core::rt::spawn_blocking(move || {
                            veles_core::rustplugins::run_gateway_filters(&p, &c, &t)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            tracing::error!("rust gateway filter panicked or was cancelled: {e}");
                            None
                        })
                    } else {
                        None
                    };
                    if let Some(d) = filter_decision {
                        match d {
                            veles_core::rustplugins::FilterDecision::Skip => return,
                            veles_core::rustplugins::FilterDecision::Rewrite(t) => text = t,
                            veles_core::rustplugins::FilterDecision::Allow => {}
                        }
                    }
                    // The chat cap gates every turn (the check below the
                    // lock); a sender without it has no turn to park
                    // behind and no turn to run, so a parked row of theirs
                    // would only ever be refused later. Refuse now.
                    if !principal.has(veles_core::acl::Cap::Chat) {
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(
                                &msg,
                                format!(
                                    "[acl] agent turns are not available to {} — ask the owner for access.",
                                    principal.label()
                                ),
                                MessageKind::Reply,
                            ),
                        );
                        return;
                    }
                    let agent = match self.agent_for_sender(
                        &msg.platform,
                        &msg.chat_id,
                        msg.sender_id.as_deref(),
                        &msg.sender_ctx,
                    ) {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::error!("park failed (could not resolve session): {e}");
                            return;
                        }
                    };
                    // Record WHERE this row's answer belongs. The drain
                    // runs it at a turn boundary, where the `msg` in
                    // scope is the turn-STARTING message: answering with
                    // that message's id put the reply under an unrelated
                    // earlier post (live: a Bluesky answer landed two
                    // posts up-thread from the question it answered), and
                    // the `parked_inbound` re-injection had no real id at
                    // all, so atproto refused to thread it and the answer
                    // was dropped without being posted.
                    match self.store.lock_ok().queue_push_routed(
                        &agent.session_id,
                        &text,
                        "message",
                        msg.msg_id.as_deref(),
                        msg.route.as_deref(),
                    ) {
                        Ok(seq) => {
                            // A Step, never a Reply: this is the
                            // framework's own queue acknowledgement, not
                            // the turn's answer. atproto posts only a
                            // Reply — the same filter that keeps a turn's
                            // progress narration off a public feed — so a
                            // Reply here published "queued #1 — a turn is
                            // running; it runs next" as a post on
                            // Bluesky. IRC, telegram and the desktop all
                            // render a Step, so the sender still hears it
                            // on a surface that has somewhere to put it.
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!("queued #{seq} — a turn is running; it runs next"),
                                    MessageKind::Step,
                                ),
                            );
                        }
                        Err(e) => {
                            tracing::error!("queue_push failed: {e}");
                            // Same reasoning as the ack above, and more
                            // so: this carries a Rust error chain, which
                            // has no business on a public feed. It is an
                            // operator-facing failure, and the log has
                            // the detail.
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!("[veles] could not queue: {e:#}"),
                                    MessageKind::Step,
                                ),
                            );
                        }
                    }
                    return;
                }
            }
        } else {
            lock.lock().await
        };

        // Builtin slash commands — answered directly, no LLM turn, no
        // session created. Parity with hermes' builtins; the point is
        // lifecycle visibility on every platform (telegram/irc/tui).
        let trimmed = msg.text.trim_start();
        if let Some(cmd) = trimmed.strip_prefix('/') {
            let mut words = cmd.split_whitespace();
            let cmd = words.next().unwrap_or("");
            // Subcommand arguments ("/subagents cancel 3") travel with
            // the whole text to the shared dispatcher, which parses its
            // own rest.
            // ACL: builtin command tiers (guest floor = status/version/
            // help; guards = cmd-status; acl = cmd-admin).
            if let Some(cap) = veles_core::acl::builtin_cap(cmd) {
                if !principal.has(cap) {
                    self.bus.send_outbound_to(
                        &msg.platform,
                        msg.route.as_deref(),
                        reply_plain(
                            &msg,
                            format!(
                                "[acl] `{cmd}` requires a higher access level ({}) — denied for {}.{}",
                                cap.as_str(),
                                principal.label(),
                                veles_core::ticketed::sudo_hint(&principal, cap)
                            ),
                            MessageKind::Reply,
                        ),
                    );
                    return;
                }
            }
            let reply = match cmd {
                "status" => Some(self.status_text()),
                // Guards are per-agent state: resolve (or create) the
                // session's agent so the snapshot is real, not a config
                // echo. Falls back to config defaults if the agent
                // can't be built.
                // Filtered to the CALLER: a guest asking for help gets
                // the commands they can run plus a count of the ones
                // they cannot, not a menu that refuses them one at a
                // time.
                // On a public feed a non-owner may run only a few
                // commands (`public_feed_refusal`); the menu says so
                // instead of listing what the feed would refuse.
                "help"
                    if veles_core::bus::is_public_feed(&msg.platform)
                        && principal.role != veles_core::acl::Role::Owner =>
                {
                    Some(PUBLIC_FEED_HELP.to_string())
                }
                "help" => Some(format!(
                    "{}\n— type anything else to chat.",
                    super::commands::help_text_for(
                        super::commands::Surface::Gateway,
                        Some(&principal)
                    )
                )),
                // ACL introspection: who am I, what can I do? (Letta
                // /whoami parity — users in shared channels need this.)
                //
                // BARE only. `/acl explain …`, `/acl grant …` and the
                // rest need the session's agent (the store the runtime
                // rules live in) and the resolved principal, which is
                // what the shared dispatcher below is handed — so an
                // argument falls through to it rather than being
                // swallowed here.
                "acl" | "access" if words.next().is_none() => {
                    Some(self.acl_self_report(&principal, &msg))
                }
                // S12: context window + budget fill (Letta /context
                // parity). Session-scoped, so cmd-status tier.
                _ => None,
            };
            if let Some(reply) = reply {
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
        }

        // ── S9: /pair — pairing-code redemption + owner management ──
        // Runs BEFORE the chat-cap gate: a guest redeeming a code has
        // no chat cap yet (that's the whole point). Owner subcommands
        // (new/list/revoke) require Cap::All.
        if let Some(rest) = trimmed.strip_prefix("/pair") {
            let rest = rest.trim();
            let reply = self.handle_pair(rest, &principal, &msg);
            self.bus.send_outbound_to(
                &msg.platform,
                msg.route.as_deref(),
                reply_plain(&msg, reply, MessageKind::Reply),
            );
            return;
        }

        // ── ACL: the chat cap gates every turn ──
        // Guests (unverified or below `user`) get a refusal instead of a
        // turn. Legacy no-config installs keep the old behavior (any
        // allowlisted sender chats).
        if !principal.has(veles_core::acl::Cap::Chat) {
            self.bus.send_outbound_to(
                &msg.platform,
                msg.route.as_deref(),
                reply_plain(
                    &msg,
                    format!(
                        "[acl] agent turns are not available to {} — ask the owner for access.",
                        principal.label()
                    ),
                    MessageKind::Reply,
                ),
            );
            return;
        }

        let mut agent = match self.agent_for_sender(
            &msg.platform,
            &msg.chat_id,
            msg.sender_id.as_deref(),
            &msg.sender_ctx,
        ) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("agent_for failed: {e}");
                return;
            }
        };

        // C3: this chat's session may have left the node since it was
        // bound here — migrated, or taken over by a survivor while this
        // node was away. Its owner gone: take it back over (lazy
        // failover). Living on elsewhere: the chat carries on in a new
        // session here and says so — the fence would refuse every
        // message in the old one, and a chat has nowhere else to go.
        match veles_core::takeover::bring_here(
            self.store.clone(),
            self.session_cfg(),
            &agent.session_id,
        )
        .await
        {
            Ok(veles_core::takeover::Brought::TookOver(line)) => {
                tracing::warn!("failover: {line}");
                let sid = agent.session_id.clone();
                match self
                    .rebind_session(
                        &msg.platform,
                        &msg.chat_id,
                        msg.sender_id.as_deref(),
                        &msg.sender_ctx,
                        &sid,
                    )
                    .await
                {
                    Ok(a) => agent = a,
                    Err(e) => tracing::warn!("failover: rebuilding {sid} failed: {e:#}"),
                }
            }
            Ok(veles_core::takeover::Brought::Elsewhere { name, .. }) => {
                let old = agent.session_id.clone();
                let sid = crate::chat::new_session_id();
                let title = agent.store.lock_ok().title(&old).ok().flatten();
                // Taken apart from the match: a guard in the scrutinee
                // would live across the await below.
                let created = agent.store.lock_ok().create_session(&sid);
                let rebound = match created {
                    Ok(()) => {
                        if let Some(t) = title {
                            let _ = agent.store.lock_ok().set_title(&sid, &t);
                        }
                        self.rebind_session(
                            &msg.platform,
                            &msg.chat_id,
                            msg.sender_id.as_deref(),
                            &msg.sender_ctx,
                            &sid,
                        )
                        .await
                    }
                    Err(e) => Err(e),
                };
                match rebound {
                    Ok(a) => {
                        agent = a;
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(
                                &msg,
                                format!(
                                    "(session {old} lives on node {name} now — this chat \
                                     continues in a new session here, {sid})"
                                ),
                                MessageKind::Reply,
                            ),
                        );
                    }
                    Err(e) => tracing::warn!("chat session {old} moved to {name}: {e:#}"),
                }
            }
            Ok(veles_core::takeover::Brought::Here) => {}
            Err(e) => tracing::debug!("where does {} live? {e:#}", agent.session_id),
        }

        // `/retry` replaces the turn text with the previous prompt. Set
        // inside the command block below, consumed by the turn after it.
        let mut retry_body: Option<String> = None;

        // A chat ALIAS (`/qwen-flash <prompt>`) mints a session, points
        // it at the alias route, and then runs the REST of the line as
        // the turn. Same shape as /retry: set inside the command block,
        // consumed by the turn after it.
        let mut alias_body: Option<String> = None;

        // Shared engine-state commands — /plan, /mech, /body and the rest
        // of `commands::SHARED_COMMANDS`. Placed here, after agent
        // resolution, for the same reason as /failover below: they need
        // the session's agent.
        //
        // It also runs AFTER the builtin table above, which is what lets
        // the gateway keep its own `/acl`: on a messaging surface the
        // sender is not necessarily the owner, so "what may THIS sender
        // do" is a different and better answer than the owner-surface
        // listing the REPL and TUI want. `commands::GATEWAY_OVERRIDES`
        // records that deliberately.
        //
        // The note that used to sit here claimed one place served
        // "telegram, irc, tui and desktop". It served the messaging
        // adapters. The TUI and the REPL run their agent in-process and
        // each kept a private copy, which is exactly the drift
        // commands.rs exists to end.
        {
            let (head, rest) = match trimmed.split_once(' ') {
                Some((h, r)) => (h, r.trim()),
                None => (trimmed, ""),
            };
            // The REAL compaction, on the chat surfaces too — IRC and
            // Telegram had no /compact at all, while the REPL and TUI
            // faked theirs (archive_head with no summarizer). Same
            // engine path the automatic trigger and the desktop's
            // /compress use. This block runs BEFORE the shared
            // dispatch because it is a declared GATEWAY_OVERRIDE:
            // compaction here is `.await`ed in async context, which a
            // sync dispatcher cannot do.
            if head == "/compress" || head == "/compact" {
                let (sid, store) = (agent.session_id.clone(), agent.store.clone());
                let mut hist = store.lock_ok().load_history(&sid).unwrap_or_default();
                let before = hist.len();
                let reply = match agent.compact_forced(&mut hist).await {
                    // The shared formatter. This arm is the one that used
                    // to answer "compacted: 514 → 433 messages" — true
                    // and useless, because it said nothing about the 81
                    // messages actually archived or the giant turn the
                    // tail still held. The outcome now names the prunes
                    // and says plainly when the target was missed.
                    Some(outcome) => outcome.report(before, hist.len()),
                    // A summary that failed is not "nothing to compact".
                    None => match agent.take_compaction_failure() {
                        Some(why) => format!("compaction failed — the history is unchanged: {why}"),
                        None => {
                            "nothing to compact — history too short for a legal cut".to_string()
                        }
                    },
                };
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // `/sandbox` — async because the live half of the answer
            // is podman's. The owner surfaces block on a fresh
            // runtime; here we are already in one, so it is awaited
            // (`GATEWAY_OVERRIDES`). Same implementation either way.
            if head == "/sandbox" || head == "/sandboxes" {
                let rest = if head == "/sandboxes" && rest.is_empty() {
                    "list"
                } else {
                    rest
                };
                let reply = super::commands::sandbox_command(
                    &super::commands::AclCtx::from_agent(&agent, Some(&principal)),
                    rest,
                )
                .await;
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // `/gen` — one media generation straight from the sender,
            // no turn. Awaited here (`GATEWAY_OVERRIDES`); the reply
            // carries a `MEDIA:` path like any reply, so the
            // connector's delivery — and its gate — handle the rest.
            if head == "/gen" || head == "/generate" {
                let reply = super::commands::gen_command(
                    &super::commands::AclCtx::from_agent(&agent, Some(&principal)),
                    rest,
                )
                .await;
                // The SAME media gate a turn's replies go through.
                // `media::enforce_outbound` lives in the turn closure
                // below, and this branch returns before reaching it — so
                // a `MEDIA:` marker leaving here was never judged. Every
                // `/gen` route writes under `config::media_dir()`, so
                // nothing was reachable; a gate that holds only because
                // of what the current back ends happen to do is a gate
                // waiting for a new back end.
                let (reply, withheld) =
                    veles_core::media::enforce_outbound(&reply, Some(&principal));
                if !withheld.is_empty() {
                    tracing::warn!(
                        "[acl] withheld {} outbound MEDIA marker(s) in a /gen reply for {} ({}): {}",
                        withheld.len(),
                        principal.subject,
                        principal.role.as_str(),
                        withheld.join(", ")
                    );
                }
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // `/tcl` — evaluated here, above the turn brakes and above
            // the turn loop, so it never reaches a provider. The IRC
            // sigil has always been answered before the bus; this is the
            // same command by its literal spelling, which until now fell
            // through to the model as a prompt.
            if head == "/tcl" || head == "/eval" || head == "/tcladmin" {
                // On a PUBLIC surface the raw eval output is a Terms-of-
                // Service exposure, and what the reply may carry is the
                // public-reply POLICY's decision (the slopdrop module's
                // ledger and output check, R5). With no policy loaded
                // nothing of the output is posted — not even evaluated —
                // and the feed is told why (fail closed).
                if veles_core::bus::is_public_feed(&msg.platform) {
                    let Some(policy) = veles_core::tcleval::public_reply() else {
                        tracing::warn!(
                            "tcl: public /tcl on {} refused: no public-reply policy is loaded",
                            msg.platform
                        );
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(
                                &msg,
                                veles_core::tcleval::PUBLIC_REPLY_UNAVAILABLE,
                                MessageKind::Reply,
                            ),
                        );
                        return;
                    };
                    let answer = super::commands::tcl_command_as_hashed(
                        &super::commands::AclCtx::from_agent(&agent, Some(&principal))
                            .with_sender(&msg.sender_ctx),
                        rest,
                        head == "/tcladmin",
                    )
                    .await;
                    let cfg = self.session_cfg();
                    let shaped = policy
                        .shape(veles_core::tcleval::PublicReplyRequest {
                            cfg: &cfg,
                            msg: &msg,
                            code: rest,
                            output: &answer.text,
                            invoked_hash: answer.invoked_hash.as_deref(),
                            spec: answer.spec.as_ref(),
                        })
                        .await;
                    let mut o = reply_plain(&msg, shaped.text, MessageKind::Reply);
                    o.meta.extend(shaped.meta);
                    self.bus
                        .send_outbound_to(&msg.platform, msg.route.as_deref(), o);
                    return;
                }
                let reply = super::commands::tcl_command_as(
                    &super::commands::AclCtx::from_agent(&agent, Some(&principal))
                        .with_sender(&msg.sender_ctx),
                    rest,
                    head == "/tcladmin",
                )
                .await;
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // `/triggers` — same GATEWAY_OVERRIDES shape: the whole
            // command is async (refresh/mask talk to slopdrop over
            // HTTP), so the gateway awaits the one implementation
            // rather than letting the sync dispatcher block a runtime
            // inside this one.
            if head == "/triggers" || head == "/trigger" {
                let reply = super::commands::triggers_command_async(
                    &super::commands::AclCtx::from_agent(&agent, Some(&principal)),
                    &agent.cfg,
                    rest,
                )
                .await;
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // `/session` — B6 migration: the wire legs (push/query)
            // are async, so the gateway awaits the shared handler
            // rather than letting the sync dispatcher block a runtime
            // inside this one (GATEWAY_OVERRIDES).
            if head == "/session" {
                let mut ctx = crate::commands::CommandCtx {
                    agent: &agent,
                    surface: crate::commands::Surface::Gateway,
                    history: None,
                    turn_live: false,
                    principal: Some(&principal),
                };
                let reply = super::commands::session_migration_command(&mut ctx, rest).await;
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // A module's DIRECT command (R5: `/slopdrop`): answered here
            // like a builtin, with the caller's principal and surface,
            // and never rendered as a prompt. Async, so the gateway
            // awaits it rather than letting the sync dispatcher block a
            // runtime inside this one.
            let bare = head.trim_start_matches('/');
            if let Some(d) = veles_core::commands::direct_command(bare) {
                let reply = veles_core::commands::run_direct(
                    bare,
                    &d,
                    veles_core::commands::DirectCall {
                        args: rest.to_string(),
                        principal: Some(principal.clone()),
                        sender: Some(msg.sender_ctx.clone()),
                        session_id: agent.session_id.clone(),
                    },
                )
                .await;
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // `/quota` — the upstream plans' credits and windows, same
            // shape again: HTTP to the vendors, so the gateway awaits
            // the one implementation instead of letting the sync arm
            // build a runtime inside this one.
            if head == "/quota" {
                // The chat's OWN agent names which plan is "active":
                // gateway chats are chat-backed sessions with a route
                // of their own, and a personal-plan chat must not be
                // told the z.ai pool's numbers are its session's.
                let (active, _, _, _) = agent.effective_model();
                let reply = super::commands::quota_command(
                    &super::commands::AclCtx::from_agent(&agent, Some(&principal)),
                    &agent.cfg,
                    rest,
                    Some(active.as_str()),
                )
                .await;
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // `/models` — a provider's catalog: discovery may fetch it
            // over HTTP, so awaited here like `/quota`. The SESSION's
            // provider is the default, as for `/quota`'s active plan.
            if head == "/models" {
                let (provider, model, _, _) = agent.effective_model();
                let reply =
                    super::commands::models_command(&agent.cfg, (&provider, &model), rest).await;
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, reply, MessageKind::Reply),
                );
                return;
            }
            // Session lifecycle: /new, /resume, /retry are the
            // DUPLICATED_ACROSS_SURFACES family. They need the owned
            // `Arc<Agent>` (a session switch swaps what the chat points
            // at), which the shared dispatcher's `&Agent` cannot do —
            // so they lived here all along but were never implemented,
            // and `/new` fell through to the model as a literal prompt
            // (the 2026-09-07 IRC hang). `dispatch` gets the rest.
            if head == "/new" || head == "/reset" {
                // A fresh session has been shown nothing, so the
                // adapter's incremental-injection mark for this
                // conversation has to go with it — otherwise the new
                // session's first turn is handed only the lines that
                // arrived since the OLD one last looked, which is the
                // one case where "only what is new" is the wrong
                // answer.
                if let Some(key) =
                    veles_core::irc_tap::context_mark_key_for(&msg.sender_ctx, &msg.chat_id)
                {
                    veles_core::irc_tap::forget_context_mark(&key);
                }
                let sid = crate::chat::new_session_id();
                let created = agent.store.lock_ok().create_session(&sid);
                match created {
                    Ok(()) => match self
                        .rebind_session(
                            &msg.platform,
                            &msg.chat_id,
                            msg.sender_id.as_deref(),
                            &msg.sender_ctx,
                            &sid,
                        )
                        .await
                    {
                        Ok(_) => {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!("(new session {sid})"),
                                    MessageKind::Reply,
                                ),
                            );
                        }
                        Err(e) => {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!("[veles] new session failed: {e:#}"),
                                    MessageKind::Reply,
                                ),
                            );
                        }
                    },
                    Err(e) => {
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(
                                &msg,
                                format!("[veles] new session failed: {e:#}"),
                                MessageKind::Reply,
                            ),
                        );
                    }
                }
                return;
            }
            // A CHAT alias — `/qwen-flash summarise this`.
            //
            // It belongs beside `/new` and not in the shared dispatcher
            // for the same reason `/new` does: it swaps the owned
            // `Arc<Agent>` this chat points at, and `CommandCtx` holds
            // only a `&Agent`. Media aliases have no such problem and are
            // a plain `dispatch_async` arm.
            //
            // The order below is the whole of it, and each step is load
            // bearing:
            //   1. drop the context mark — a fresh session has been shown
            //      nothing, so "only what is new" is the wrong answer
            //      (same reasoning as /new above);
            //   2. mint and rebind, so the NEXT mention continues this
            //      session rather than the one the alias replaced;
            //   3. set the model on the FRESH agent. `frontends::fork`
            //      carries the previous session's overrides forward, so
            //      skipping this leaves the new session on the old
            //      route — the alias would appear to work and quietly
            //      bill the wrong provider;
            //   4. hand the rest of the line to the turn.
            if let Some(a) = veles_core::aliases::resolve_command(&agent.cfg, head) {
                if a.kind == veles_core::aliases::AliasKind::Chat {
                    if let Some(key) =
                        veles_core::irc_tap::context_mark_key_for(&msg.sender_ctx, &msg.chat_id)
                    {
                        veles_core::irc_tap::forget_context_mark(&key);
                    }
                    let sid = crate::chat::new_session_id();
                    let minted = agent
                        .store
                        .lock_ok()
                        .create_session(&sid)
                        .map_err(|e| anyhow::anyhow!("{e:#}"));
                    let outcome = match minted {
                        Err(e) => Err(e),
                        Ok(()) => {
                            self.rebind_session(
                                &msg.platform,
                                &msg.chat_id,
                                msg.sender_id.as_deref(),
                                &msg.sender_ctx,
                                &sid,
                            )
                            .await
                        }
                    };
                    let fresh = match outcome {
                        Ok(f) => f,
                        Err(e) => {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!("[veles] /{}: new session failed: {e:#}", a.name),
                                    MessageKind::Reply,
                                ),
                            );
                            return;
                        }
                    };
                    // `model@provider`, the unambiguous spelling — see
                    // `Alias::model_arg`.
                    let route = match fresh.set_model(&a.model_arg()) {
                        Ok(msg) => msg,
                        Err(e) => {
                            // The session exists and is bound, so say so
                            // rather than pretending nothing happened —
                            // the caller is now IN it, on the inherited
                            // route, and needs to know which.
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!(
                                        "[veles] /{}: new session {sid}, but the route did not \
                                         resolve: {e:#} — it is on the inherited model",
                                        a.name
                                    ),
                                    MessageKind::Reply,
                                ),
                            );
                            return;
                        }
                    };
                    if let Some(r) = &a.reasoning {
                        fresh.set_reasoning(r);
                    }
                    // Announce. Minting a session and changing route is
                    // exactly the kind of implicit action that must be
                    // visible — "nothing implicit", and /new already sets
                    // this precedent by printing the id it made.
                    self.bus.send_outbound_to(
                        &msg.platform,
                        msg.route.as_deref(),
                        reply_plain(
                            &msg,
                            format!("(new session {sid} · {route})"),
                            MessageKind::Reply,
                        ),
                    );
                    // Bare `/qwen-flash` is "put me on this route", the
                    // way `/new` is "put me somewhere fresh". Nothing to
                    // run, and an empty turn would bill for nothing.
                    if rest.trim().is_empty() {
                        return;
                    }
                    agent = fresh;
                    alias_body = Some(rest.to_string());
                }
            }
            if head == "/resume" {
                if rest.is_empty() {
                    self.bus.send_outbound_to(
                        &msg.platform,
                        msg.route.as_deref(),
                        reply_plain(&msg, "usage: /resume <id-or-prefix>", MessageKind::Reply),
                    );
                    return;
                }
                let found = {
                    let store = agent.store.lock_ok();
                    store.find_session(rest).unwrap_or_default()
                };
                match found {
                    Some(id) => match self
                        .rebind_session(
                            &msg.platform,
                            &msg.chat_id,
                            msg.sender_id.as_deref(),
                            &msg.sender_ctx,
                            &id,
                        )
                        .await
                    {
                        Ok(_) => {
                            let n = agent
                                .store
                                .lock_ok()
                                .load_history(&id)
                                .ok()
                                .map(|h| h.len());
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    match n {
                                        Some(n) => format!("(resumed {id}, {n} messages)"),
                                        None => format!("(resumed {id})"),
                                    },
                                    MessageKind::Reply,
                                ),
                            );
                        }
                        Err(e) => {
                            self.bus.send_outbound_to(
                                &msg.platform,
                                msg.route.as_deref(),
                                reply_plain(
                                    &msg,
                                    format!("[veles] resume failed: {e:#}"),
                                    MessageKind::Reply,
                                ),
                            );
                        }
                    },
                    None => {
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(
                                &msg,
                                format!("no session matching '{rest}'"),
                                MessageKind::Reply,
                            ),
                        );
                    }
                }
                return;
            }
            if head == "/retry" {
                match agent.last_prompt() {
                    None => {
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(&msg, "nothing to retry", MessageKind::Reply),
                        );
                        return;
                    }
                    Some(last) => {
                        self.bus.send_outbound_to(
                            &msg.platform,
                            msg.route.as_deref(),
                            reply_plain(&msg, "(retrying last prompt…)", MessageKind::Reply),
                        );
                        retry_body = Some(last);
                    }
                }
            }
            let mut ctx = crate::commands::CommandCtx {
                agent: &agent,
                surface: crate::commands::Surface::Gateway,
                // The gateway loads history from the store per turn;
                // there is no live vec to hand over.
                history: None,
                // Commands run under the per-chat lock, which a running
                // turn holds — so by construction no turn is live here.
                turn_live: false,
                principal: Some(&principal),
            };
            let shared = crate::commands::dispatch(&mut ctx, head, rest);
            if let Some(handled) = shared {
                self.bus.send_outbound_to(
                    &msg.platform,
                    msg.route.as_deref(),
                    reply_plain(&msg, handled.text, MessageKind::Reply),
                );
                return;
            }
        }

        // pre_gateway_dispatch hook (parity): plugins may skip/rewrite.
        // `/retry` replays the previous prompt; a chat alias runs the
        // rest of its own line. Both replace the turn text, and neither
        // can be set at the same time as the other (an alias arm returns
        // or falls through; /retry returns).
        let mut text = alias_body
            .take()
            .or_else(|| retry_body.take())
            .unwrap_or_else(|| msg.text.clone());
        // ── ACL: a marker is a file channel, so it gets the question the
        // file tools already answer ──
        // `MEDIA:/path` inbound is not decoration: every round resolves it
        // into a real read (`agent.rs` → `media::resolve`, a plain
        // `std::fs::read` of whatever the marker names). Before this, any
        // role holding `chat` — guests included — could read any file the
        // gateway can, and a missing one answered "no such file", which is
        // an existence oracle for arbitrary host paths. Below owner, only the
        // media root and the sender's own sandbox workspace are honoured, and
        // the rest is replaced with a note HERE, before a plugin filter, a
        // custom command or the turn itself can resolve it — so the file is
        // never opened and the marker answers nothing. Owner surfaces
        // (TUI/REPL/desktop/CLI) never come through this seam, which is why
        // `/image` still works for the owner.
        let (defanged, inbound_withheld) =
            veles_core::media::enforce_inbound(&text, Some(&principal));
        if !inbound_withheld.is_empty() {
            tracing::warn!(
                "[acl] withheld {} inbound MEDIA marker(s) from {} ({}): {}",
                inbound_withheld.len(),
                principal.subject,
                principal.role.as_str(),
                inbound_withheld.join(", ")
            );
        }
        text = defanged;
        // S6: custom commands (/name args → rendered template) become
        // the turn text. TRUSTED-TIER ONLY: templates may inline files
        // and run shell snippets (`!`cmd``) — rendering one for an
        // untrusted sender would execute on THIS host. User/guest
        // senders fall through with the raw text (the model sees the
        // literal "/cmd" as before).
        if text.starts_with('/') && principal.has(veles_core::acl::Cap::CmdSession) {
            let head = text
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if let Some(resolved) = veles_core::commands::find_any(&head) {
                let args = text
                    .trim_start_matches('/')
                    .trim_start_matches(&head)
                    .trim();
                let rendered = match resolved {
                    veles_core::commands::ResolvedCommand::Disk(c) => {
                        tracing::info!(
                            "gateway: custom command /{} rendered ({} chars)",
                            c.name,
                            args.len()
                        );
                        Some(veles_core::commands::render(&c, args))
                    }
                    // Async surface: await the Tcl hop, never block.
                    veles_core::commands::ResolvedCommand::Plugin(t) => {
                        match veles_core::commands::render_plugin_async(&t, args).await {
                            Ok(s) => {
                                tracing::info!(
                                    "gateway: plugin command /{} rendered ({} chars)",
                                    t.name,
                                    s.len()
                                );
                                Some(s)
                            }
                            Err(e) => {
                                tracing::warn!("gateway: plugin command /{} failed: {e}", t.name);
                                None
                            }
                        }
                    }
                };
                if let Some(r) = rendered {
                    text = r;
                }
            }
        }
        if let Some(r) = self.hooks_fire_gateway(&msg).await {
            match r {
                GatewayDecision::Skip => return,
                GatewayDecision::Rewrite(t) => text = t,
                GatewayDecision::Allow => {}
            }
        }
        // Rust plugin filters run after the Tcl hook (plugin plan Part
        // 2): the Tcl plugin gets the first word, a registered rust
        // `gateway_filter` the next. Same decision shape.
        //
        // On a blocking thread, for the reason spelled out in
        // `HookBus::fire`: a filter is sync by contract and may call
        // back into the engine, and this runs on every inbound message.
        let filter_decision = if veles_core::rustplugins::has_gateway_filter() {
            let (p, c, t) = (msg.platform.clone(), msg.chat_id.clone(), text.clone());
            veles_core::rt::spawn_blocking(move || {
                veles_core::rustplugins::run_gateway_filters(&p, &c, &t)
            })
            .await
            .unwrap_or_else(|e| {
                tracing::error!("rust gateway filter panicked or was cancelled: {e}");
                None
            })
        } else {
            None
        };
        if let Some(d) = filter_decision {
            match d {
                veles_core::rustplugins::FilterDecision::Skip => return,
                veles_core::rustplugins::FilterDecision::Rewrite(t) => text = t,
                veles_core::rustplugins::FilterDecision::Allow => {}
            }
        }

        // The drain loop (R4.6): one turn per message, reloading
        // history each iteration so text parked by ANOTHER surface
        // while this one's turn ran is answered as its own turn at
        // this boundary too. The queue is shared: the TUI's composer,
        // the desktop's chat endpoint and this channel all write the
        // same rows, and whoever finishes the turn works the drain.
        let mut body = text;
        // The seq THIS loop's current turn owns, if any (the first turn
        // answers the inbound message itself and owns no row). Released
        // at the drain boundary below, before the next take.
        let mut cur_seq: Option<i64> = None;
        // The message THIS loop's current turn answers. The first turn
        // answers the inbound message, so it starts as a copy of it; a
        // DRAINED row replaces the parts that belong to a message
        // (`msg_id`, the threading target, and `route`, the adapter
        // instance that carried it), because it is a different message
        // that parked behind this one.
        //
        // Reading `msg.msg_id` for every iteration was the bug: on a
        // threaded platform the drained answer posted under the
        // turn-STARTING message, two messages up-thread from the question
        // it answered (observed live on Bluesky), and where the platform
        // refuses an unresolvable thread the answer was not posted at
        // all. Same for `msg.route`: a row parked on one IRC network
        // would have had its answer sent out the other. Everything inside
        // the loop reads `cur_msg`, so a new outbound site cannot
        // reintroduce the stale target.
        let mut cur_msg = msg.clone();
        // IRC channel context an adapter attached (platforms.irc.
        // inject_channel_context / observe). Persisted UNCONDITIONALLY
        // once the history is loaded — the transcript shows the ⇒
        // labelled block whatever the injection knob says. The row is
        // pushed into THIS turn's request only while the knob is on
        // (later turns re-read it from the store, exactly like the
        // engine's own suppressed injections). Consumed by the FIRST
        // turn; queued drain turns are different messages.
        // Rolling per-turn context an adapter attached (IRC's channel
        // context, atproto's surface framing + thread posts). Persisted
        // UNCONDITIONALLY once the history is loaded — the transcript
        // shows the ⇒ labelled block whatever the injection knob says.
        // The row is pushed into THIS turn's request only while the
        // platform's knob is on (later turns re-read it from the store,
        // exactly like the engine's own suppressed injections). Consumed
        // by the FIRST turn; queued drain turns are different messages.
        // The tag is platform-derived (`irc_context`, `atproto_context`)
        // so the gate and the transcript label name the right surface.
        let ctx_tag = format!("{platform}_context");
        let mut context_row: Option<ChatMessage> = msg
            .context
            .as_ref()
            .map(|ctx| ChatMessage::user(ctx).with_injection(&ctx_tag));

        // ── The two brakes that fire BEFORE the provider ──
        //
        // Placed here on purpose: past every command branch, so
        // `/status` and `/limits` are never billed as turns, and before
        // the turn loop, so a refusal costs one SQL read and nothing
        // else. The token ceiling in `Agent::over_spend_ceiling` needs a
        // measurement to compare against, so by the time it can answer,
        // the money for the calls it measured is already spent — these
        // two are what make a flood cost nothing rather than cost less.
        // Runtime overrides included: `/limits set` says "live on every
        // surface" and these brakes are a surface. Resolved BEFORE the
        // store is locked below — `with_runtime_limits` locks it too.
        //
        // The claim is INSIDE the loop, not above it: a message parked in
        // the queue must claim its OWN turn when it drains, so the brake
        // refuses a drain-time turn it was under when it arrived — a
        // message that never runs must not be billed, and one that DOES
        // run must not be free just because it was parked.

        loop {
            let acl_now = veles_core::acl::with_runtime_limits(
                if self.cfg_from_file {
                    veles_core::acl::live_config()
                } else {
                    self.cfg.acl.clone()
                },
                &self.store,
            );
            let rate = acl_now.limits.turns_per_hour_for_subject(
                principal.role,
                &principal.subject,
                &principal.budget_key(),
            );
            let refused = self
                .store
                .lock_ok()
                .claim_turn(
                    &principal.budget_key(),
                    rate,
                    veles_core::agent::TURN_RATE_WINDOW_SECS,
                )
                .unwrap_or(None);
            if let Some(retry) = refused {
                // A DRAINED message was promoted to `running`; retire it so a
                // refused turn is dropped, not re-run forever at the next
                // boundary. The first message owns no row (cur_seq is None).
                if let Some(seq) = cur_seq.take() {
                    let _ = self.store.lock_ok().queue_release(&agent.session_id, seq);
                }
                self.bus.send_outbound_to(
                    &cur_msg.platform,
                    cur_msg.route.as_deref(),
                    reply_plain(
                        &cur_msg,
                        format!(
                            "[veles] {} has started {rate} turns in the last hour, which is the \
                             ceiling for {}. Try again in {retry}s — an admin can raise it with \
                             `/limits turns-per-hour {} <n>`.",
                            principal.subject,
                            principal.role.as_str(),
                            principal.role.as_str()
                        ),
                        MessageKind::Reply,
                    ),
                );
                return;
            }
            let limit = acl_now.limits.concurrent_turns_for_role(principal.role);
            let _turn_permit = match self.turn_permit(&principal.subject, limit) {
                Ok(permit) => permit,
                Err(n) => {
                    if let Some(seq) = cur_seq.take() {
                        let _ = self.store.lock_ok().queue_release(&agent.session_id, seq);
                    }
                    self.bus.send_outbound_to(
                        &cur_msg.platform,
                        cur_msg.route.as_deref(),
                        reply_plain(
                            &cur_msg,
                            format!(
                                "[veles] {} already has {n} turns running, which is the limit. \
                                 Wait for one to finish (or `/cancel` it) and say that again.",
                                principal.subject
                            ),
                            MessageKind::Reply,
                        ),
                    );
                    return;
                }
            };
            let mut history: Vec<ChatMessage> = {
                let store = agent.store.clone();
                let sid = agent.session_id.clone();
                let loaded = store.lock_ok().load_history(&sid);
                loaded.unwrap_or_default()
            };
            if let Some(mut row) = context_row.take() {
                // Stamped: compaction archives what it summarized by id.
                row.msg_id = agent.persist(&row);
                if agent.cfg.agent.injections.rolling_context(&platform) {
                    history.push(row);
                }
            }

            // Mid-turn progress events (hermes tool_progress parity):
            // the adapter gets a typing indicator at turn start and
            // editable "⚙ tool…" status messages per tool round. Sent
            // through the same outbound channel as the final reply —
            // the adapter splits on MessageKind.
            let addressee = address_name(&cur_msg.sender, &cur_msg.sender_ctx);
            // The owner, joined from another chat, sees what starts this
            // turn and everything it says.
            self.mirror_turn_text(&agent, &body);
            let progress_tx = self.with_tees(
                self.voice(
                    &platform,
                    cur_msg.route.as_deref(),
                    &cur_msg.chat_id,
                    cur_msg.sender_ctx.direct,
                    &turn_key,
                    addressee.clone(),
                ),
                &agent.session_id,
            );
            if let Some(tx) = &progress_tx {
                // empty text = typing indicator only
                let _ = tx.send(reply_plain(&cur_msg, String::new(), MessageKind::Progress));
            }

            let mut reasoning_display: Option<String> = None;
            // Streaming progress: accumulated deltas emitted at most every
            // 800ms as Progress messages carrying partial text. Adapters
            // with streaming display live-edit a status message with it;
            // adapters without streaming ignore Progress-with-text.
            let mut stream_buf = String::new();
            let mut last_progress = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(10))
                .unwrap_or_else(std::time::Instant::now);
            // Intermediate assistant message from a multi-round turn:
            // (content, display_reasoning). The FINAL round's message is
            // held here and delivered as the Reply; a round followed by
            // another (a fresh Done, or a ToolStart) is flushed as a Step
            // so the chat sees each step as the model produces it rather
            // than a single dump at the end.
            let mut pending: Option<(String, Option<String>)> = None;
            // ── ACL: the media gate, once per turn ──
            // A `MEDIA:` marker is an instruction to a connector to hand over
            // a file — telegram uploads it, IRC publishes it and links it —
            // and it can ride ANY line this turn sends: the reply, an
            // intermediate step, even a streaming partial. So the gate is one
            // closure every send goes through, rather than one check on the
            // path that looked like the only one. Until now the only check
            // anywhere on the path was `is_file()`, and the system prompt
            // teaches EVERY principal the marker: the distance between a
            // `user` role and `MEDIA:/home/user/.config/veles/.env` in a
            // public channel was the model's discretion. Below owner, only the
            // media root and the sender's own sandbox workspace survive; the
            // channel gets a note with no path in it and the log gets the path
            // and the principal. Enforced HERE rather than per connector
            // because this is the one seam that knows who is asking, and a
            // connector that had to know roles would be a second ACL to keep
            // in step with the first.
            let gate = |text: &str, what: &str| -> String {
                let (text, withheld) = veles_core::media::enforce_outbound(text, Some(&principal));
                if !withheld.is_empty() {
                    tracing::warn!(
                        "[acl] withheld {} outbound MEDIA marker(s) in a {what} for {} ({}): {}",
                        withheld.len(),
                        principal.subject,
                        principal.role.as_str(),
                        withheld.join(", ")
                    );
                }
                text
            };
            let res = {
                let a = &agent;
                let chat_id = cur_msg.chat_id.clone();
                // This core's windows, not the process's. The closure
                // outlives the borrow of `self` that made it, which is
                // why the field is an Arc.
                let progress = Arc::clone(&self.progress);
                a.run_turn(&mut history, &body, None, |ev| match ev {
                    StreamEvent::Done(m) => {
                        reasoning_display =
                            m.display_reasoning.clone().or_else(|| m.reasoning.clone());
                        // A fresh round's stream finished: reset the
                        // live-edit buffer so the next round does not
                        // re-send this round's already-flushed text.
                        stream_buf.clear();
                        // A NEW Done means whatever was held was NOT the
                        // final answer — flush it before replacing it.
                        if let Some((content, reason)) = pending.take() {
                            send_step(&progress_tx, &chat_id, gate(&content, "step"), reason);
                        }
                        pending = Some((
                            m.content.clone(),
                            m.display_reasoning.clone().or_else(|| m.reasoning.clone()),
                        ));
                    }
                    StreamEvent::Delta(t) => {
                        stream_buf.push_str(&t);
                        if last_progress.elapsed() >= std::time::Duration::from_millis(800) {
                            last_progress = std::time::Instant::now();
                            if let Some(tx) = &progress_tx {
                                let _ = tx.send(OutboundMessage::plain(
                                    &chat_id,
                                    gate(&stream_buf, "streaming partial"),
                                    MessageKind::Progress,
                                ));
                            }
                        }
                    }
                    StreamEvent::ToolStart(name, args) => {
                        // The round that just finished chose more work via
                        // a tool: its message is intermediate, flush it
                        // NOW — the tool run itself can be long.
                        if let Some((content, reason)) = pending.take() {
                            send_step(&progress_tx, &chat_id, gate(&content, "step"), reason);
                        }
                        // Aggregated: a FULL window narrates itself when
                        // its sixth call lands; anything shorter rides the
                        // tail flush before the reply.
                        if let Some(line) = note_progress(&progress, &turn_key, &name, &args) {
                            if let Some(tx) = &progress_tx {
                                let _ = tx.send(OutboundMessage::plain(
                                    &chat_id,
                                    gate(&line, "tool summary"),
                                    MessageKind::Progress,
                                ));
                            }
                        }
                    }
                    StreamEvent::Note(n) => {
                        // A note is its own message, sent where it happened
                        // — not batched to the head of the reply after a
                        // multi-tool turn has finished narrating. The
                        // `⚙` tool-summary line above is the precedent:
                        // same bus, same pacing. Step (not Progress) so the
                        // live-edit-away that once justified the batching
                        // does not swallow it; the per-connector filter
                        // still trims one platform's copy, never the
                        // mechanism. The media gate still applies: a note
                        // is text going to the wire.
                        if let Some(tx) = progress_tx.as_ref().filter(|_| {
                            !agent
                                .cfg
                                .mechanisms
                                .silences_on(&cur_msg.platform, n.kind.as_str())
                        }) {
                            let _ = tx.send(OutboundMessage::plain(
                                &chat_id,
                                gate(&n.line(), "note"),
                                MessageKind::Step,
                            ));
                        }
                    }
                    _ => {}
                })
                .await
            };

            let out_tx = self.with_tees(
                self.voice(
                    &platform,
                    cur_msg.route.as_deref(),
                    &cur_msg.chat_id,
                    cur_msg.sender_ctx.direct,
                    &turn_key,
                    addressee,
                ),
                &agent.session_id,
            );
            // The turn's tool narration closes with the reply: whatever
            // the window still holds — even a partial one — counts and
            // is sent once, then the window goes away for good.
            if let Some(line) = tail_progress(&self.progress, &turn_key) {
                if let Some(tx) = &out_tx {
                    let _ = tx.send(reply_plain(
                        &cur_msg,
                        gate(&line, "tool summary"),
                        MessageKind::Progress,
                    ));
                }
            }
            // Read AFTER the turn, not before: `present_plan` can flip the
            // mode mid-turn, and a reply that says "nothing was changed"
            // when the approval just let work begin would be a lie.
            let plan_mode_now = {
                let sid = agent.session_id.clone();
                self.store
                    .lock()
                    .ok()
                    .map(|s| veles_core::plan::mode_for(&s, &sid) == veles_core::plan::Mode::Plan)
                    .unwrap_or(false)
            };

            match res {
                Ok(TurnResult {
                    reply,
                    provider,
                    model,
                    interrupted,
                    ..
                }) => {
                    if let Some(tx) = out_tx {
                        // A pending message is the model's last round. It
                        // normally IS the reply; flush it as a Step only
                        // when the turn replaced the final answer with
                        // something else (a guard halt, an interrupt), so
                        // nothing the model wrote is lost on the way out.
                        if let Some((content, reason)) = pending.take() {
                            if !content.trim().is_empty() && content.trim() != reply.trim() {
                                send_step(
                                    &Some(tx.clone()),
                                    &cur_msg.chat_id,
                                    gate(&content, "step"),
                                    reason,
                                );
                            }
                        }
                        // The plan-mode marker for the chat platforms. irc
                        // and telegram have no status bar to put it in, so
                        // it rides the reply — otherwise the only surfaces
                        // that can say "this session changed nothing" are
                        // the two with chrome, and the two where a user is
                        // most likely to forget are the two that stay
                        // silent.
                        let mut text = if plan_mode_now {
                            format!("▲ [plan mode — nothing was changed]\n{reply}")
                        } else {
                            reply
                        };
                        // A cancelled turn delivers its partial reply; say
                        // it was cut short, the way the TUI and CLI do.
                        if interrupted {
                            text.push_str("\n[interrupted]");
                        }
                        // The turn's media gate (defined above, with the
                        // reasoning): last thing that touches the text before
                        // it becomes a message.
                        let text = gate(&text, "reply");
                        let _ = tx.send(OutboundMessage {
                            chat_id: cur_msg.chat_id.clone(),
                            // Reply at the LEVEL of the post that mentioned
                            // us: thread the triggering message id back out
                            // so a threaded adapter (atproto) answers the
                            // specific mention, not the conversation root.
                            reply_to: cur_msg.msg_id.clone(),
                            text,
                            reasoning: reasoning_display,
                            provider,
                            model,
                            kind: MessageKind::Reply,
                            ask: None,
                            meta: Default::default(),
                        });
                    }
                }
                Err(e) => {
                    if let Some(tx) = out_tx {
                        let _ = tx.send(reply_plain(
                            &cur_msg,
                            format!("[veles] error: {e:#}"),
                            MessageKind::Reply,
                        ));
                    }
                }
            }
            // The turn boundary drains the shared queue (R4.6): the next
            // parked message becomes its own turn here, in seq order.
            // `queue_take_next` promotes a pending row to `running` and
            // demotes one whose (dead) taker left it behind, so a crashed
            // drainer cannot strand a session's queue.
            //
            // Release OUR completed row first: turn end cleared the
            // session_runs row, so a stale `running` row left standing
            // reads as a crash and the same message re-runs forever
            // (2026-09-08 flood). Only the seq this loop took.
            if let Some(seq) = cur_seq.take() {
                let _ = self
                    .store
                    .lock()
                    .ok()
                    .map(|s| s.queue_release(&agent.session_id, seq));
            }
            match self.store.lock().ok().and_then(|s| {
                s.queue_take_next(&agent.session_id)
                    .map_err(|e| {
                        tracing::warn!(
                            "queue: taking {}'s next row failed: {e:#}",
                            agent.session_id
                        )
                    })
                    .ok()
                    .flatten()
            }) {
                Some(item) => {
                    body = item.body;
                    cur_seq = Some(item.seq);
                    // The next turn answers THIS row's message, not the
                    // one that started the turn whose boundary we are on.
                    // Overwrite the two fields that belong to a message:
                    // `msg_id` (the threading target) and `route` (the
                    // adapter instance that carried it). Everything else —
                    // platform, chat_id, sender — is the session's own, and
                    // every row in one session's queue arrived on them.
                    //
                    // A mail or cron row parked behind no platform message,
                    // so both are NULL: `reply_to` then falls back to the
                    // session's own chat, which is where such an answer
                    // belongs.
                    cur_msg.msg_id = item.reply_to.clone();
                    cur_msg.route = item.route.clone();
                    continue;
                }
                None => break,
            }
        }
    }

    async fn hooks_fire_gateway(&self, msg: &InboundMessage) -> Option<GatewayDecision> {
        // Minimal parity hook: pre_gateway_dispatch may return
        // {"action":"skip"} | {"action":"rewrite","text":...} | allow.
        //
        // Fired on the SHARED bus, and stamped with the session this
        // sender-in-chat resolves to. The comment here used to say no
        // session exists yet for a chat's first message — that was true
        // of the hook's original placement, not of this one:
        // `agent_for_sender` runs above (the command block and the turn
        // are both below it), so the session does exist and a handler
        // that calls back into the engine can be judged as THIS sender
        // instead of whoever booted last. An unresolvable key stamps ""
        // and the bridge falls back, exactly as before.
        let key = self.session_key_for(
            &msg.platform,
            &msg.chat_id,
            msg.sender_id.as_deref(),
            &msg.sender_ctx,
        );
        let sid = self
            .session_ids
            .lock_ok()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let r = self
            .hooks
            .fire(
                "pre_gateway_dispatch",
                &veles_core::hooks::with_session(
                    &sid,
                    serde_json::json!({
                        "platform": msg.platform,
                        "chat_id": msg.chat_id,
                        "sender": msg.sender,
                        "text": msg.text,
                    }),
                ),
            )
            .await?;
        let v: serde_json::Value = serde_json::from_str(&r).ok()?;
        match v.get("action").and_then(|a| a.as_str())? {
            "skip" => Some(GatewayDecision::Skip),
            "rewrite" => Some(GatewayDecision::Rewrite(
                v.get("text").and_then(|t| t.as_str())?.to_string(),
            )),
            _ => Some(GatewayDecision::Allow),
        }
    }
}

impl Gateway {
    /// Run the gateway loop (call after starting adapter tasks).
    pub async fn run(mut self) -> Result<()> {
        let core = self.core.clone();
        core.restore_gateway_joins();
        // VAULT IDENTITY, once per start (not per sync tick): a node whose
        // key files were regenerated still looks like a member by its
        // node id, and every vault lookup quietly falls back to the env
        // files (pitfall 76). `veles doctor` and `veles vault status`
        // say the same at length.
        veles_core::rt::spawn(async {
            match veles_core::rt::spawn_blocking(veles_core::vault::local_health).await {
                Ok(Ok(Some(h))) if h.is_problem() => {
                    tracing::warn!("vault: {}", h.lines().join("; "))
                }
                Ok(Err(e)) => tracing::warn!("vault: checking this node's identity failed: {e:#}"),
                _ => {}
            }
        });
        // CLUSTER REGISTRY (Plan B, B1): this gateway's host is a compute
        // node in the pool. Register it (identity + capability probe) at
        // boot, then heartbeat on the cluster cadence so the rest of the
        // pool — and `veles nodes` — can tell a live worker from a
        // sleeping laptop. Registration is best-effort: a gateway whose
        // store is momentarily contended still serves chat, it just
        // shows up stale in the registry until the next tick.
        //
        // All of it runs in the BACKGROUND, and the probe runs WITHOUT the
        // store lock. The first version awaited registration inside run()
        // and probed nvidia-smi (no deadline) while holding the store
        // mutex, so a wedged driver stalled the boot and every store user
        // behind it. Nothing below needs the node id.
        {
            let store = core.store.clone();
            let cfg = core.cfg.clone();
            let interval = core.cfg.cluster.gossip_interval_secs.max(5);
            veles_core::rt::spawn(async move {
                let me = match veles_core::rt::spawn_blocking(move || {
                    veles_core::node::resolve_self(&cfg)
                })
                .await
                {
                    Ok(Ok(me)) => std::sync::Arc::new(me),
                    Ok(Err(e)) => {
                        tracing::warn!("cluster: node identity/probe failed: {e:#}");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("cluster: node probe task failed: {e:#}");
                        return;
                    }
                };
                let register =
                    |store: Arc<std::sync::Mutex<veles_core::store::Store>>,
                     me: std::sync::Arc<veles_core::node::SelfNode>| async move {
                        veles_core::rt::spawn_blocking(move || {
                            let s = store.lock().unwrap_or_else(|p| p.into_inner());
                            veles_core::node::register_resolved(&s, &me)
                        })
                        .await
                    };
                match register(store.clone(), me.clone()).await {
                    Ok(Ok(())) => {
                        tracing::info!("cluster: registered node {} ({})", me.identity.id, me.name)
                    }
                    Ok(Err(e)) => tracing::warn!("cluster: node registration failed: {e:#}"),
                    Err(e) => tracing::warn!("cluster: node registration task failed: {e:#}"),
                }
                // B5: the store log + its triggers, only while
                // `[cluster] storesync_peers` names someone — and removed
                // when it names nobody (the log copies every message it
                // sees, and a node with no peers has no reader for it).
                // Best-effort, same posture as registration.
                {
                    let (peers, days) = veles_core::config::load()
                        .map(|c| (c.cluster.storesync_peers, c.cluster.storesync_backfill_days))
                        .unwrap_or_default();
                    let s = store.lock().unwrap_or_else(|p| p.into_inner());
                    match veles_core::storesync::sync_log_install(&s, &peers, days) {
                        Ok(state) => tracing::debug!("cluster: store log {state}"),
                        Err(e) => tracing::warn!("cluster: store log install failed: {e:#}"),
                    }
                }
                veles_core::rt::spawn(storesync_push_loop(store.clone(), me.clone(), interval));
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
                tick.tick().await; // first tick is immediate — skip it, we just registered
                                   // Gossip peers that did not answer last time. The phone
                                   // stops five minutes after its last use (--exit-when-idle),
                                   // so "down" is its normal state: said once, not every tick.
                let mut down = std::collections::BTreeSet::<String>::new();
                loop {
                    tick.tick().await;
                    let s = store.clone();
                    let id = me.identity.id.clone();
                    // B4: the load sample rides the heartbeat write —
                    // same tick, same row, one lock. Read on THIS side
                    // of the spawn_blocking boundary (off the async
                    // runtime's threads, though /proc is cheap).
                    let load = veles_core::rt::spawn_blocking(veles_core::node::probe_load_one)
                        .await
                        .ok()
                        .flatten()
                        .map(|l| l.value);
                    let r = veles_core::rt::spawn_blocking(move || {
                        s.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .node_heartbeat(&id, load)
                    })
                    .await;
                    match r {
                        // The row is gone (`veles nodes forget` on this
                        // node): come back rather than heartbeat into the
                        // void until the next restart.
                        Ok(Ok(0)) => {
                            tracing::info!("cluster: own registry row missing — re-registering");
                            let _ = register(store.clone(), me.clone()).await;
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!("cluster: heartbeat failed: {e:#}"),
                        Err(e) => tracing::warn!("cluster: heartbeat task failed: {e:#}"),
                    }
                    // B3a gossip: on the same cadence, exchange registry
                    // rows with every configured gossip peer
                    // ([cluster] gossip_peers names [a2a_agents] entries).
                    // Empty list = off (the default: gossip is opt-in
                    // exactly like memsync — nothing dials out unasked).
                    let peers = {
                        match veles_core::config::load() {
                            Ok(c) => c.cluster.gossip_peers.clone(),
                            Err(_) => Vec::new(),
                        }
                    };
                    for peer_name in peers {
                        let (url, bearer) = match peer_target(&peer_name) {
                            Some(t) => t,
                            None => {
                                tracing::warn!(
                                    "cluster: gossip peer '{peer_name}' is not in \
                                     [a2a_agents] — skipping"
                                );
                                continue;
                            }
                        };
                        let store2 = store.clone();
                        let r = veles_core::gossip::exchange_with_peer(
                            &store2,
                            &peer_name,
                            &url,
                            bearer.as_deref(),
                        )
                        .await;
                        match r {
                            Ok(line) => {
                                tracing::debug!("cluster: gossip {line}");
                                if down.remove(&peer_name) {
                                    // Back (the phone, opened again): what
                                    // it is owed goes on the next sync tick,
                                    // not after a backoff of up to 30 min
                                    // that would outlast its five-minute stay.
                                    veles_core::backoff::pool().reset_peer(&peer_name);
                                    tracing::info!(
                                        "cluster: {peer_name} answers again — pushing to it \
                                         from the next sync tick"
                                    );
                                }
                            }
                            Err(e) => {
                                if down.insert(peer_name.clone()) {
                                    tracing::warn!(
                                        "cluster: gossip with {peer_name} failed: {e:#} \
                                         (quiet until it answers again)"
                                    )
                                } else {
                                    tracing::debug!(
                                        "cluster: gossip with {peer_name} failed: {e:#}"
                                    )
                                }
                            }
                        }
                    }
                }
            });
        }
        // Boot announcement: "▶ veles gateway up" to the last-known chat
        // on each platform (restart should never be silent). Delayed 1s
        // so the adapters have registered their outbound channels.
        {
            let bus = core.bus.clone();
            veles_core::rt::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let sent = bus.broadcast_notice(&format!(
                    "▶ veles gateway up (v{})",
                    env!("CARGO_PKG_VERSION")
                ));
                if sent > 0 {
                    tracing::info!("announced boot to {sent} platform(s)");
                }
            });
        }
        // Cron scheduler: shares the store, config, and hook bus with all
        // gateway sessions; delivers through the same outbound channels.
        // Started before the inbound loop consumes `self`.
        //
        // B3a: the scheduler is a singleton duty. The lease acquire
        // lives HERE (not veles-cli) because the scheduler is spawned
        // by the gateway itself. With the fence armed the scheduler
        // always runs, gated at every tick by the guard's local
        // deadline (`lease_held`, no network round trip): a refused
        // node is a STANDBY and fires nothing until it is granted.
        let scheduler = veles_core::cron::Scheduler::new(
            core.store.clone(),
            core.cfg.clone(),
            core.hooks.clone(),
            Some(core.bus.clone()),
        );
        {
            let cfg = core.cfg.clone();
            let store = core.store.clone();
            let sched = scheduler.clone();
            veles_core::rt::spawn(async move {
                use veles_core::arbiter::LeaseGuard;
                let pool_declared = veles_core::node::pool_declared(&cfg.cluster);
                let arbiter_set = cfg
                    .cluster
                    .arbiter
                    .as_deref()
                    .is_some_and(|a| !a.trim().is_empty());
                let stops_when_idle = veles_core::idle::limit().is_some();
                if veles_core::cron::cron_duty(arbiter_set || pool_declared, stops_when_idle)
                    == veles_core::cron::CronDuty::Own
                {
                    // Single-node mode: nobody to double with, ungated. Or
                    // a member that stops when idle (the phone): it never
                    // asks for the pool's lease — a holder that stops five
                    // minutes after its last use is no holder, and asking
                    // is a poll the radio pays for — and its cron state is
                    // its own, so it fires its own jobs, and its service's
                    // alarm starts it for the next one
                    // (device_events::write_wake at the idle exit).
                    if stops_when_idle {
                        tracing::info!(
                            "cluster: this gateway stops when idle (--exit-when-idle): it never \
                             asks for the pool's cron lease and fires its own cron jobs, ungated"
                        );
                    }
                    sched.run_loop().await;
                    return;
                }
                // A pool member: the scheduler runs gated by a guard from
                // the start — ticks skip until this node HOLDS the cron
                // lease (a standby takes over once a holder's lease runs
                // out; B3a returned on the first refusal, so cron moved
                // to no node when its holder died, and one renewal blip
                // stopped it for good).
                let me = veles_core::node::NodeIdentity::load_or_create(
                    &veles_core::config::config_dir(),
                )
                .map(|i| i.id)
                .unwrap_or_default();
                let key = veles_core::arbiter::lease_key_cron();
                let guard = Arc::new(LeaseGuard::standby(&key, &me));
                sched.set_lease(guard.clone());
                {
                    let sched = sched.clone();
                    veles_core::rt::spawn(async move { sched.run_loop().await });
                }
                // Not keyed on `uptime_class`: that defaults to
                // "ephemeral" for every node that does not set it — the
                // laptop does not — and the laptop is the VPS's cron
                // standby. (A gateway that stops when idle took the Own
                // branch above.)
                // Arm the fence, retrying while it cannot: a FRESH node
                // has not heard of the arbiter until the first gossip
                // lands. B3a refused once and never looked again, so a
                // new node's cron stayed off until a restart.
                let every = std::time::Duration::from_secs(
                    (veles_core::arbiter::LEASE_TTL_SECS / 3).max(5) as u64,
                );
                let mut said = false;
                let arb = loop {
                    match veles_core::arbiter::StaticArbiter::new(store.clone(), cfg.clone()) {
                        Ok(a) => break a,
                        Err(e) => {
                            if !said {
                                tracing::warn!(
                                    "cron scheduler on STANDBY: the fence cannot arm yet — {e:#} \
                                     (retrying every {}s; a new node learns the arbiter by gossip)",
                                    every.as_secs()
                                );
                                said = true;
                            }
                            tokio::time::sleep(every).await;
                        }
                    }
                };
                let arb: Arc<dyn veles_core::arbiter::Arbiter> = Arc::new(arb);
                guard.ask_now(arb.as_ref()).await;
                if guard.held() {
                    tracing::info!("cluster: holding the cron scheduler lease");
                } else {
                    tracing::warn!(
                        "cron scheduler on STANDBY: lease not granted ({}) — \
                         it runs when this node is granted the lease",
                        guard.lost_reason().unwrap_or_default()
                    );
                }
                guard.spawn_renewal(arb);
            });
        }

        // Checkpoint retention (M13): one sweep at start. Long-lived
        // gateways are exactly where blobs pile up.
        {
            let store = core.store.clone();
            veles_core::rt::spawn_blocking(move || match veles_core::checkpoints::prune(&store) {
                Ok((0, _)) => {}
                Ok((n, freed)) => tracing::info!(
                    "checkpoints: pruned {n} ({:.1} MB freed)",
                    freed as f64 / (1024.0 * 1024.0)
                ),
                Err(e) => tracing::warn!("checkpoint prune failed: {e:#}"),
            });
        }

        // IRC scrollback: drain the tap into the store, and trim on a
        // daily tick. The writer runs whether or not an IRC adapter is
        // configured — subscribing to a bus nobody publishes on costs
        // one parked task, and starting it conditionally would mean
        // knowing about veles-irc, which core deliberately does not.
        {
            let store = core.store.clone();
            veles_core::rt::spawn(veles_core::irc_tap::run_store_writer(store));
        }
        // The slopdrop bridge's timer poll runs in its module (R5).
        // Token-plan quota (2026-09-15): warm the cache in the
        // background so the desktop status field and `/quota` read a
        // local snapshot instead of paying a Singapore round trip on a
        // UI path. `poll_secs = 0` disables it, and the cache is then
        // filled only by whoever asks first.
        {
            let cfg = core.cfg.clone();
            veles_core::rt::spawn(async move {
                run_tokenplan_poller(cfg).await;
            });
        }
        // The ask that follows the owner off the surface it was born
        // on (PENDING §"Cross-session clarify relay").
        install_owner_ask_relay(&core.cfg, &core.bus, &core.store);
        // Background completion wake turns (2026-09-13, USER's
        // contract): a `shell-complete`/`monitor-fired` for an idle
        // chat-backed session starts an agent turn in it; a busy
        // session's running turn already drains the completion at its
        // next step. One loop, spawned once — the gateway owns every
        // chat-backed session for the process's life.
        {
            let owner = Arc::new(GatewayWakeOwner {
                core: core.clone(),
                bus: core.bus.clone(),
            });
            veles_core::rt::spawn(veles_core::session_wake::run_wake_loop(owner));
        }
        {
            // ONCE AT BOOT is not retention. PRUNE_PER_BUFFER and
            // PRUNE_DAYS were enforced exactly when the process
            // started, so on a gateway that stays up for a month —
            // which is the normal case, and the case retention exists
            // for — the table grew without any bound at all. First
            // sweep immediately, then daily.
            let store = core.store.clone();
            veles_core::rt::spawn(async move {
                loop {
                    let s = store.clone();
                    let _ = veles_core::rt::spawn_blocking(move || {
                        let s = match s.lock() {
                            Ok(s) => s,
                            Err(p) => p.into_inner(),
                        };
                        match s.irc_prune(
                            veles_core::irc_tap::PRUNE_PER_BUFFER,
                            veles_core::irc_tap::PRUNE_DAYS,
                        ) {
                            Ok(0) => {}
                            Ok(n) => tracing::info!("irc scrollback: pruned {n} lines"),
                            Err(e) => tracing::warn!("irc scrollback prune failed: {e:#}"),
                        }
                    })
                    .await;
                    tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
                }
            });
        }

        // A2A inbound server (platforms.a2a.enabled, axum-gated). Runs in
        // its own task; failures log but don't take the gateway down.
        #[cfg(feature = "frontend-gateway-http")]
        if crate::a2a_server::a2a_enabled(&core.cfg) {
            let srv = crate::a2a_server::A2AServer::new(
                core.cfg.clone(),
                core.store.clone(),
                core.hooks.clone(),
            );
            veles_core::rt::spawn(async move {
                if let Err(e) = crate::a2a_server::serve(srv).await {
                    tracing::error!("A2A server failed: {e:#}");
                }
            });
        } else {
            tracing::info!("A2A platform disabled (platforms.a2a.enabled)");
        }

        // Desktop API server (platforms.desktop.enabled, axum-gated):
        // REST + SSE loopback surface for veles-desktop (Tcl/Tk). Runs
        // in its own task; failures log but don't take the gateway down.
        #[cfg(feature = "frontend-gateway-http")]
        if crate::desktop_api::desktop_enabled(&core.cfg) {
            let srv = crate::desktop_api::DesktopServer::new(
                core.cfg.clone(),
                core.store.clone(),
                core.hooks.clone(),
            )
            .with_bus(core.bus.clone())
            .with_scheduler(scheduler.clone());
            veles_core::rt::spawn(async move {
                if let Err(e) = crate::desktop_api::serve(Arc::new(srv)).await {
                    tracing::error!("desktop API server failed: {e:#}");
                }
            });
        } else {
            tracing::info!("desktop API disabled (platforms.desktop.enabled)");
        }

        // OpenAI-compatible proxy (platforms.proxy.enabled, axum-gated):
        // #34 in the feature matrix — any OpenAI client points at
        // /v1/chat/completions and gets veles' provider chain. Runs in
        // its own task; failures log but don't take the gateway down.
        #[cfg(feature = "frontend-gateway-http")]
        if veles_core::proxy::proxy_enabled(&core.cfg) {
            let cfg = core.cfg.clone();
            veles_core::rt::spawn(async move {
                if let Err(e) = veles_core::proxy::serve(&cfg).await {
                    tracing::error!("OpenAI-compatible proxy failed: {e:#}");
                }
            });
        } else {
            tracing::info!("OpenAI proxy disabled (platforms.proxy.enabled)");
        }
        // On a phone, the app asks for a loopback CONNECT proxy: git there
        // is static against musl, which cannot resolve names on Android
        // (veles_core::connect_proxy).
        if let Some((addr, token)) = veles_core::connect_proxy::spec_from_env() {
            veles_core::rt::spawn(async move {
                if let Err(e) = veles_core::connect_proxy::serve(addr, token).await {
                    tracing::warn!("connect proxy on {addr}: {e:#}");
                }
            });
        }

        // One task per inbound message (M11). The loop MUST NOT await a
        // turn: with approvals a turn can park on a human for minutes,
        // and the old inline `handle(msg).await` would have silenced
        // every other chat and platform meanwhile. Ordering within a
        // session is preserved by `GatewayCore::turn_lock`.
        //
        // SIGTERM/SIGINT → graceful shutdown: announce to every
        // connected platform, then stop draining. The process exits
        // right after, so the adapters get a moment to flush the
        // announcement before the runtime drops.
        // The listeners are the ones the module host pinned before any
        // module ran (lifecycle::pin_signal_handlers), so a stop that came
        // while the plugins started is in them; fresh ones otherwise.
        // One future for the process's life: the signals (or, on
        // Windows, the console control events) and the IPC `shutdown`
        // request, through `lifecycle::wait_for_stop`.
        let mut stop = std::pin::pin!(veles_core::lifecycle::wait_for_stop());
        // `--exit-when-idle` (the phone): checked every 15 s, or sooner
        // for a short limit; never fires without one.
        let idle_every = veles_core::idle::limit()
            .map(|l| std::time::Duration::from_secs((l / 4).clamp(1, 15)))
            .unwrap_or(std::time::Duration::from_secs(3600));
        let mut idle_tick = tokio::time::interval(idle_every);
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // What kept it up past the limit, said when it changes.
        let mut held_up: Option<String> = None;
        // Where the clock stands, every 5 minutes and on the first check:
        // a phone whose gateway outlived its limit with no use and no
        // turn in its log (2026-10-04) says from now on whether the
        // check runs at all.
        let mut checks = 0u64;
        // The gateway serves from here: the idle clock starts now, not at
        // the process start, so a boot slower than the limit is not an
        // idle gateway (veles_core::idle::ready says when it was).
        veles_core::idle::ready();
        loop {
            tokio::select! {
                _ = idle_tick.tick(), if veles_core::idle::limit().is_some() => {
                    if checks.is_multiple_of((300 / idle_every.as_secs().max(1)).max(1)) {
                        tracing::info!("idle: {}", veles_core::idle::status());
                    }
                    checks += 1;
                    if veles_core::idle::due() {
                        tracing::info!(
                            "idle for {}s with no turn running: stopping (--exit-when-idle)",
                            veles_core::idle::idle_secs()
                        );
                        // What the phone's service starts it again for
                        // (veles_core::device_events): a notification a
                        // trigger listens to, and the next cron job. A
                        // gateway that stops when idle fires its own cron
                        // (cron::cron_duty), in a pool too, so the plan
                        // names the next job's time whenever there is one.
                        let in_pool = veles_core::node::pool_declared(&core.cfg.cluster)
                            || core
                                .cfg
                                .cluster
                                .arbiter
                                .as_deref()
                                .is_some_and(|a| !a.trim().is_empty());
                        let runs_cron = veles_core::cron::cron_duty(in_pool, true)
                            == veles_core::cron::CronDuty::Own;
                        match veles_core::device_events::write_wake(&core.store, runs_cron, in_pool) {
                            Ok(w) => tracing::info!(
                                "idle: woken again for notifications from {:?}, next cron at {:?}, heartbeat at {:?}",
                                w.notifications,
                                w.at,
                                w.heartbeat_at,
                            ),
                            Err(e) => tracing::warn!("idle: could not write the wake plan: {e:#}"),
                        }
                        veles_core::idle::mark_exited();
                        break;
                    }
                    let now = veles_core::idle::held_up_by();
                    if now != held_up {
                        if let Some(turns) = &now {
                            tracing::info!(
                                "idle for {}s, but a turn is running ({turns}): staying up \
                                 until it ends",
                                veles_core::idle::idle_secs()
                            );
                        }
                        held_up = now;
                    }
                }
                msg = self.inbound_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            veles_core::idle::touch_by(|| {
                                format!("a message on {}", msg.platform)
                            });
                            let core = core.clone();
                            veles_core::rt::spawn(async move { core.handle(msg).await });
                        }
                        None => break,
                    }
                }
                _ = &mut stop => {
                    tracing::info!("signal: graceful shutdown requested");
                    let bus = core.bus.clone();
                    bus.broadcast_notice("🛑 veles gateway stopping");
                    // Let adapters flush the notice before the runtime drops.
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    break;
                }
            }
        }
        Ok(())
    }
}

enum GatewayDecision {
    Skip,
    Rewrite(String),
    Allow,
}

/// Dedup key for exactly-once inbound delivery.
///
/// Why this exists: the equivalent hermes machinery has **no** inbound
/// dedup at all. Two duplication vectors observed in hermes:
///
/// 1. **Redelivered updates.** Telegram long-polling redelivers pending
///    updates after a reconnect/crash when the client dies before the
///    getUpdates offset advances (its 409-conflict handler re-enters
///    polling with `drop_pending_updates=False`). A redelivered update
///    then runs the whole agent turn a second time — the user sees their
///    message answered twice.
/// 2. **Steer-then-replay.** While the agent is busy, a new user message
///    is injected mid-turn via `agent.steer()` (OUT-OF-BAND marker
///    appended to a tool result). The steer machinery drains
///    `_pending_steer` at up to four sites (tool_executor parallel/
///    sequential/segmented + the pre-API-call drain in
///    conversation_loop.py), and the gateway's fallback queue keeps the
///    same event for the next turn whenever steering "falls back" — so
///    the same text can land as a mid-turn marker AND be replayed as a
///    next-turn user message. Observed live: the same user text delivered
///    to the model three times in one session.
///
/// Veles closes both at the bus: one message in → one turn out. The key
/// is the platform-native message id when available (telegram ids are
/// unique per chat and strictly increasing — immune to identical-text
/// collisions), else a sender+content hash for id-less platforms. The
/// claim itself is `Store::claim_inbound` — a durable, atomic
/// INSERT-OR-IGNORE on a UNIQUE key, which is exactly the consumer-side
/// idempotency layer any at-least-once queue (RabbitMQ, NATS, Redis
/// Streams) still requires on top of its delivery. No broker needed for
/// a single-process consumer; if veles grows multi-process fan-out later,
/// the claim survives unchanged.
pub fn dedup_key(msg: &InboundMessage) -> String {
    // `dedup_id` first: it is the exactly-once key for THIS delivery, set
    // only by a re-injection that already spent the platform id in the
    // ledger (a queued row handed back through `handle`). Keying that on
    // `msg_id` would read the re-injection as a redelivery and drop the
    // answer — which is exactly why the re-injection used to fake
    // `msg_id` instead, and lose the reply's thread target with it.
    match msg.dedup_id.as_ref().or(msg.msg_id.as_ref()) {
        // Scoped per platform+chat: message ids restart per chat, so an
        // id alone would collide across chats.
        Some(id) => format!("id:{}:{}:{}", msg.platform, msg.chat_id, id),
        None => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            msg.sender.hash(&mut h);
            msg.text.hash(&mut h);
            format!("h:{}:{}:{:016x}", msg.platform, msg.chat_id, h.finish())
        }
    }
}

/// The owner's Telegram DM, from config.
///
/// `[platforms.telegram.home_channel] chat_id` is HERMES' OWN SPELLING,
/// honoured rather than replaced. veles dropped it on migration and has
/// been treating it as dead config ever since — `config_surface.rs`
/// literally uses that table as its worked example of a hermes block
/// that survives an audit as inert. The owner was right that it should
/// already have been there.
fn owner_home_chat(cfg: &veles_core::config::VelesConfig) -> Option<(String, String)> {
    let chat = cfg
        .platforms
        .get("telegram")
        .and_then(|t| t.get("home_channel"))
        .and_then(|h| h.get("chat_id"))
        .and_then(|v| match v {
            serde_yaml::Value::String(s) => Some(s.clone()),
            serde_yaml::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })?;
    let chat = chat.trim().to_string();
    if chat.is_empty() {
        return None;
    }
    Some(("telegram".to_string(), chat))
}

/// Install the off-surface leg for the owner's questions, or say why not.
///
/// Announced rather than silent, both ways: a relay nobody knows about
/// would deliver the owner's approvals to a phone without warning, and
/// a relay that is configured but inert ("I turned it on and nothing
/// happened") has to name the missing piece.
fn install_owner_ask_relay(
    cfg: &veles_core::config::VelesConfig,
    bus: &GatewayBus,
    store: &Arc<std::sync::Mutex<Store>>,
) {
    // The home chats first, for every platform that names one: the ask
    // relay's destination below, and `deliver = "home"` in cron — the
    // ops chat (USER 2026-10-21, veles_core::home).
    for (platform, chat) in veles_core::home::install(cfg) {
        tracing::info!("home chat: {platform}:{chat}");
    }
    if !cfg.notify.ask_relay {
        veles_core::ask::clear_owner_fallback();
        veles_core::askrelay::set_sink(None);
        tracing::debug!("ask relay: off ([notify] ask_relay is not true)");
        return;
    }
    let Some((platform, chat_id)) = owner_home_chat(cfg) else {
        tracing::warn!(
            "ask relay: on, but no destination — set [platforms.telegram.home_channel] \
             chat_id in config.toml (hermes wrote it as home_channel.chat_id too)"
        );
        veles_core::ask::clear_owner_fallback();
        veles_core::askrelay::set_sink(None);
        return;
    };
    let delay = cfg.notify.ask_relay_delay_secs;
    let kinds = cfg.notify.ask_relay_kinds.clone();
    let key = format!("{platform}:{chat_id}");
    let home: std::sync::Arc<dyn veles_core::ask::Asker> =
        std::sync::Arc::new(veles_core::ask::ChatAsker {
            bus: bus.clone(),
            platform: platform.clone(),
            chat_id,
            // The OWNER answers this leg. An ask relayed into a chat
            // with no sender binding reads as owner-surface, which in a
            // DM is right and would not be anywhere else; this is the
            // owner's home chat: a DM, or the private ops group (an ask
            // nobody opened there is decided by an owner alone —
            // `ask::may_answer`).
            sender_id: None,
            shared_audience: false,
        });
    // Where the home chat's adapter runs is the lease's to decide: here,
    // or on the node that holds it (askrelay.rs) — and a peer's relayed
    // question lands in this same chat when that node is this one.
    veles_core::askrelay::set_sink(Some(home.clone()));
    veles_core::ask::set_owner_fallback(
        std::sync::Arc::new(veles_core::askrelay::OwnerRelayAsker {
            local: home,
            bus: bus.clone(),
            platform,
            store: store.clone(),
        }),
        &key,
        delay,
        kinds.clone(),
    );
    tracing::info!("ask relay: an unanswered {kinds:?} follows the owner to {key} after {delay}s");
}

/// Resolve a `[cluster] gossip_peers` name to (url, bearer) through the
/// `[a2a_agents]` table — gossip dials by NAME so the credential stays
/// where every other A2A dialer keeps it. None = the name matches no
/// agent entry (said out loud by the caller).
fn peer_target(name: &str) -> Option<(String, Option<String>)> {
    let cfg = veles_core::config::load().ok()?;
    let peer = cfg.a2a_agents.get(name)?;
    let bearer = {
        let t = peer.auth.token.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    Some((peer.url.clone(), bearer))
}

/// B5 tail — AUTO store sync: push this node's log to every storesync
/// peer (per-peer cursor in meta; the same flag arms apply-at-receive on
/// the receiving side), drain the inbox, prune what every peer has.
/// Off unless `[cluster] storesync_auto`; `nothing to push` passes are
/// silent.
///
/// Its own task (2026-09-29), woken by the gossip cadence OR by a turn
/// ending (`storesync::nudge`): a replica is what a takeover starts
/// from, so it should end where the conversation did, not up to a tick
/// earlier. It used to ride the gossip loop, behind a registry exchange
/// with every gossip peer.
async fn storesync_push_loop(
    store: Arc<std::sync::Mutex<veles_core::store::Store>>,
    me: std::sync::Arc<veles_core::node::SelfNode>,
    interval: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = veles_core::storesync::nudged() => {}
        }
        let Some(cfg) = veles_core::config::load().ok() else {
            continue;
        };
        // Applied-offer archives grow forever (memsync's and
        // storesync's, `inbox/applied/`). Once a week, prune files
        // older than a week — the fail-closed rule (unprovable mtime
        // kept) lives in the two prune_* functions. Before the flag
        // checks: a node with storesync off still archives memsync.
        prune_applied_archives().await;
        let base = std::time::Duration::from_secs(interval);
        // Apply first, ask second. A receive and a tick can cross: an
        // offer sitting in the inbox is not by itself a conflict or a
        // security hold. Every enabled drain gets to finish before the
        // decision set is scanned, so the owner is never asked about an
        // echo that the normal apply path is about to retire.
        drain_sync_inboxes(
            &store,
            cfg.cluster.configsync_auto,
            cfg.cluster.memsync_auto,
        )
        .await;
        if cfg.cluster.memsync_auto {
            memsync_push(&store, &me, &cfg, base).await;
        }
        if cfg.cluster.configsync_auto && !cfg.cluster.memsync_peers.is_empty() {
            configsync_push(&store, &me, &cfg, base).await;
            vaultsync_push(&store, &cfg, base).await;
        }
        // Decisions held here for the owner — config, memory, vault,
        // queued messages — are asked where the owner is, one at a time
        // (syncask.rs; USER 2026-10-06: "I can always review everything
        // from telegram"). Nothing without `[notify] ask_relay`.
        veles_core::syncask::ask_due(store.clone(), Arc::new(cfg.clone()));
        if !cfg.cluster.storesync_auto {
            continue;
        }
        let peers = cfg.cluster.storesync_peers.clone();
        let days = cfg.cluster.storesync_backfill_days;
        let backoff = veles_core::backoff::pool();
        for peer_name in &peers {
            let Some((url, bearer)) = peer_target(peer_name) else {
                tracing::warn!(
                    "cluster: storesync peer '{peer_name}' is not in [a2a_agents] — skipping"
                );
                continue;
            };
            // A peer that refused or could not be reached waits out its
            // backoff instead of being sent the same chunk every tick.
            let key = format!("storesync:{peer_name}");
            if !backoff.ready(&key, std::time::Instant::now()) {
                continue;
            }
            // Off the loop, one pass per peer at a time: a pass pushes
            // until the peer has caught up, and the phone 5,000 rows
            // behind over LTE held the loop for minutes (2026-10-04) —
            // and with it config, vault and memory sync to every peer.
            let Some(flight) = PushInFlight::claim(peer_name) else {
                continue;
            };
            let (store, me, peer_name) = (store.clone(), me.clone(), peer_name.clone());
            veles_core::rt::spawn(async move {
                let _flight = flight;
                let r = veles_core::storesync::push_incremental_to_peer(
                    &store,
                    &me.identity.id,
                    &me.name,
                    &peer_name,
                    &url,
                    bearer.as_deref(),
                    days,
                )
                .await;
                let backoff = veles_core::backoff::pool();
                match r {
                    Ok(line) => {
                        backoff.succeeded(&key);
                        if line != "nothing to push" {
                            tracing::info!("storesync: {peer_name}: {line}")
                        }
                    }
                    Err(e) => {
                        let wait = backoff.failed(&key, base, std::time::Instant::now());
                        tracing::warn!(
                            "storesync: auto-push to {peer_name} failed (next try in {}s): {e:#}",
                            wait.as_secs()
                        )
                    }
                }
            });
        }
        // A receiver that was down missed apply windows: drain what its
        // peers re-pushed, and heal drift for offers stored while this
        // node had auto off (the operator may have hand-stored offers
        // before flipping it on). SQLite work: off the runtime's threads.
        let (applied, refusals) =
            match veles_core::rt::spawn_blocking(veles_core::storesync::apply_inbox).await {
                Ok(r) => r,
                // The runtime is dropping its blocking tasks: the gateway
                // is stopping (the phone's idle stop, every restart), and
                // calling that "a refusal pending" sent the reader looking
                // for a refusal that was never there.
                Err(e) if e.is_cancelled() => return,
                Err(e) => {
                    tracing::warn!("storesync: applying the inbox failed: {e}");
                    (0, Vec::new())
                }
            };
        if applied > 0 {
            tracing::info!("storesync: drained {applied} inbox offer(s)");
        }
        // Takeovers of THIS node's sessions ride in the replicas just
        // applied (C2): check each, hand the session over, keep what was
        // written here meanwhile as a fork.
        let arbiter = veles_core::arbiter::StaticArbiter::new(store.clone(), Arc::new(cfg))
            .ok()
            .map(std::sync::Arc::new);
        match veles_core::takeover::learn(store.clone(), arbiter).await {
            Ok(lines) => {
                for (settled, l) in lines {
                    if settled {
                        tracing::warn!("failover: {l}");
                    } else {
                        tracing::debug!("failover: {l}");
                    }
                }
            }
            Err(e) if stopping(&e) => return,
            Err(e) => tracing::warn!("failover: checking takeovers failed: {e:#}"),
        }
        // What every peer has applied is never needed again — the log
        // keeps only the tail someone still lacks.
        // A batch per lock hold, so other store users get in between.
        let s = store.clone();
        let pruned = veles_core::rt::spawn_blocking(move || -> anyhow::Result<usize> {
            use veles_core::storesync::{prune_log, PRUNE_BATCH};
            let mut total = 0;
            loop {
                let n = prune_log(
                    &veles_core::store::lock_timed(&s, "storesync::prune_log"),
                    &peers,
                    PRUNE_BATCH,
                )?;
                total += n;
                if n < PRUNE_BATCH {
                    return Ok(total);
                }
            }
        })
        .await;
        if let Ok(Err(e)) = pruned {
            tracing::warn!("storesync: pruning the log failed: {e:#}");
        }
        if let Some(r) = refusals.first() {
            tracing::warn!(
                "storesync: {} refusal(s) pending in the inbox — first: {}",
                refusals.len(),
                r.msg
            );
        }
    }
}

/// A store push pass under way to one peer; the next tick skips that
/// peer until it ends (dropped, also on a panic or a cancelled task).
struct PushInFlight(String);

static PUSHES_IN_FLIGHT: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

impl PushInFlight {
    fn claim(peer: &str) -> Option<Self> {
        let mut set = PUSHES_IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner());
        set.insert(peer.to_string()).then(|| Self(peer.to_string()))
    }
}

impl Drop for PushInFlight {
    fn drop(&mut self) {
        PUSHES_IN_FLIGHT
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

/// An error that is only the runtime cancelling a blocking task: the
/// gateway is stopping, and the loop that asked has nothing to report.
fn stopping(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<tokio::task::JoinError>()
            .is_some_and(|j| j.is_cancelled())
    })
}

/// The weekly `inbox/applied/` sweep. Nothing reads an applied file
/// once the offer landed, and a held conflict or refusal is NEVER moved
/// into `applied/`, so the archive is safe to prune from the loop side.
/// Process-local once-a-week (a restart re-arms it — pruning a few
/// files early is harmless).
async fn prune_applied_archives() {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST_PRUNE: AtomicI64 = AtomicI64::new(0);
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    let week = 7 * 24 * 3600;
    if now.as_secs() as i64 - LAST_PRUNE.load(Ordering::Relaxed) < week {
        return;
    }
    LAST_PRUNE.store(now.as_secs() as i64, Ordering::Relaxed);
    let mem_home = veles_core::memsync::inbox_dir();
    let store_home = veles_core::storesync::inbox_dir();
    let mem_removed =
        veles_core::rt::spawn_blocking(move || veles_core::memsync::prune_applied(&mem_home, week))
            .await
            .unwrap_or(0);
    let store_removed = veles_core::rt::spawn_blocking(move || {
        veles_core::storesync::prune_applied(&store_home, week)
    })
    .await
    .unwrap_or(0);
    // Config and vault offers apply on arrival now, so their archives
    // grow with every edit. Same layout (`applied/*.json`), same rule.
    let other_removed = veles_core::rt::spawn_blocking(move || {
        veles_core::memsync::prune_applied(&veles_core::configsync::inbox_dir(), week)
            + veles_core::memsync::prune_applied(&veles_core::vault::inbox_dir(), week)
    })
    .await
    .unwrap_or(0);
    if mem_removed + store_removed + other_removed > 0 {
        tracing::info!(
            "inbox applied-prune: removed {mem_removed} memsync + {store_removed} storesync + \
             {other_removed} config/vault file(s)"
        );
    }
}

/// `[cluster] configsync_auto`, the sending half: log this node's own
/// config edits (a scan, when the file changed since the last one) and
/// push each `memsync_peers` peer the rows it has not had, from a
/// per-peer cursor, with the same backoff as the other pushes.
///
/// USER 2026-10-04: "Config sync is manual push? Not what I expected" —
/// `veles configsync scan`/`push` and `apply --inbox` on the other end
/// were the only way a config edit travelled.
async fn configsync_push(
    store: &Arc<std::sync::Mutex<veles_core::store::Store>>,
    me: &veles_core::node::SelfNode,
    cfg: &veles_core::config::VelesConfig,
    base: std::time::Duration,
) {
    static SEEN: std::sync::Mutex<Option<std::time::SystemTime>> = std::sync::Mutex::new(None);
    let path = veles_core::configsync::config_path();
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let changed = {
        let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
        let c = *seen != mtime;
        *seen = mtime;
        c
    };
    if changed {
        let (s, id, p) = (store.clone(), me.identity.id.clone(), path.clone());
        let scanned = veles_core::rt::spawn_blocking(move || {
            let s = veles_core::store::lock_timed(&s, "configsync::scan");
            veles_core::configsync::scan_file(&s, &id, &p)
        })
        .await;
        match scanned {
            Ok(Ok(r)) if !r.logged.is_empty() || !r.tombstoned.is_empty() => {
                tracing::info!(
                    "configsync: logged {} local edit(s)",
                    r.logged.len() + r.tombstoned.len()
                )
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                // Look again next tick.
                *SEEN.lock().unwrap_or_else(|p| p.into_inner()) = None;
                tracing::warn!("configsync: scanning config.toml failed: {e:#}");
            }
            Err(e) => tracing::warn!("configsync: scan task failed: {e}"),
        }
    }
    let backoff = veles_core::backoff::pool();
    for peer in &cfg.cluster.memsync_peers {
        let key = veles_core::configsync::cursor_key(peer);
        let built = {
            let s = store.lock().unwrap_or_else(|p| p.into_inner());
            let cursor: i64 = s.meta_get(&key).and_then(|v| v.parse().ok()).unwrap_or(0);
            s.config_log_max_seq().and_then(|high| {
                veles_core::configsync::build_offer(&s, &me.identity.id, &me.name, cursor)
                    .map(|(offer, _withheld)| (offer, high))
            })
        };
        let (offer, high) = match built {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("configsync: building {peer}'s offer failed: {e:#}");
                continue;
            }
        };
        if offer.rows.is_empty() {
            continue;
        }
        let Some((url, bearer)) = peer_target(peer) else {
            tracing::warn!("cluster: configsync peer '{peer}' is not in [a2a_agents] — skipping");
            continue;
        };
        let bkey = format!("configsync:{peer}");
        if !backoff.ready(&bkey, std::time::Instant::now()) {
            continue;
        }
        match veles_core::configsync::send_offer(&url, bearer.as_deref(), &offer).await {
            Ok(reply) => {
                backoff.succeeded(&bkey);
                if let Err(e) = store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .meta_set(&key, &high.to_string())
                {
                    tracing::warn!("configsync: saving {peer}'s cursor failed: {e:#}");
                }
                let said = reply.split("\n\n").nth(1).unwrap_or(&reply);
                tracing::info!(
                    "configsync: pushed {} row(s) to {peer}: {}",
                    offer.rows.len(),
                    said.trim()
                );
            }
            Err(e) => {
                let wait = backoff.failed(&bkey, base, std::time::Instant::now());
                tracing::warn!(
                    "configsync: auto-push to {peer} failed (next try in {}s): {e:#}",
                    wait.as_secs()
                );
            }
        }
    }
}

/// The config and vault inboxes, applied from the loop — but only when
/// a file is there that this process has not tried yet. A held offer
/// (a security section, a conflict, a vault this node lacks) is retried
/// when something new arrives or the gateway restarts, and said once,
/// not every tick.
async fn drain_sync_inboxes(
    store: &Arc<std::sync::Mutex<veles_core::store::Store>>,
    config_auto: bool,
    memory_auto: bool,
) {
    static TRIED: std::sync::Mutex<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>)> =
        std::sync::Mutex::new((Vec::new(), Vec::new()));
    let config = veles_core::configsync::inbox_pending(&veles_core::configsync::inbox_dir())
        .unwrap_or_default();
    let vault =
        veles_core::vault::inbox_pending(&veles_core::vault::inbox_dir()).unwrap_or_default();
    let (new_config, new_vault) = {
        let mut tried = TRIED.lock().unwrap_or_else(|p| p.into_inner());
        let fresh = |seen: &[std::path::PathBuf], now: &[std::path::PathBuf]| {
            now.iter().any(|p| !seen.contains(p))
        };
        let out = (fresh(&tried.0, &config), fresh(&tried.1, &vault));
        *tried = (config, vault);
        out
    };
    if config_auto && new_config {
        let s = store.clone();
        let line = veles_core::rt::spawn_blocking(move || {
            crate::a2a_server::apply_configsync_inbox(&veles_core::store::lock_timed(
                &s,
                "configsync::apply_inbox",
            ))
        })
        .await;
        if let Ok(line) = line {
            tracing::debug!("configsync: drained the inbox: {line}");
        }
    }
    if config_auto && new_vault {
        let line = veles_core::rt::spawn_blocking(crate::a2a_server::apply_vault_inbox).await;
        if let Ok(line) = line {
            tracing::debug!("vault: drained the inbox: {line}");
        }
    }
    if memory_auto {
        let line = crate::a2a_server::apply_memsync_inbox().await;
        tracing::debug!("memsync: drained the inbox before decisions: {line}");
    }
}

/// `[cluster] configsync_auto` carries the vault too, the sending half:
/// when this node's vault holds rows a `memsync_peers` peer is not known
/// to have, the whole file goes to it — it is small, and a merge is
/// idempotent. What a peer has is the digest of the last offer it
/// stored from here or sent here (`vault::rows_digest`), so an
/// unchanged vault sends nothing, and the merge of a peer's offer that
/// added nothing here is not sent back to it. A node without a vault
/// sends nothing.
///
/// USER 2026-10-04: "let's wrap this all up, including automatic vault
/// push" — `veles vault push <peer>` and `apply --inbox` on the other
/// end were the only way a vault write travelled.
async fn vaultsync_push(
    store: &Arc<std::sync::Mutex<veles_core::store::Store>>,
    cfg: &veles_core::config::VelesConfig,
    base: std::time::Duration,
) {
    let v = match veles_core::rt::spawn_blocking(veles_core::vault::Vault::open_local_if_any).await
    {
        Ok(Ok(Some(v))) if v.is_initialized() => v,
        Ok(Ok(_)) => return,
        Ok(Err(e)) => {
            tracing::warn!("vault: reading this node's vault failed: {e:#}");
            return;
        }
        Err(_) => return,
    };
    let digest = veles_core::vault::rows_digest(&v.file);
    let backoff = veles_core::backoff::pool();
    for peer in &cfg.cluster.memsync_peers {
        let key = veles_core::vault::peer_has_key(peer);
        let has = store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .meta_get(&key);
        if has.as_deref() == Some(digest.as_str()) {
            continue;
        }
        let Some((url, bearer)) = peer_target(peer) else {
            tracing::warn!("cluster: vault peer '{peer}' is not in [a2a_agents] — skipping");
            continue;
        };
        let bkey = format!("vaultsync:{peer}");
        if !backoff.ready(&bkey, std::time::Instant::now()) {
            continue;
        }
        let msg = veles_core::vault::Message::Offer {
            from_node: v.keys.node.clone(),
            from_name: v.keys.name.clone(),
            file: v.file.clone(),
        };
        match veles_core::vault::push_to_peer(&msg, &url, bearer.as_deref()).await {
            Ok(reply) => {
                backoff.succeeded(&bkey);
                if let Err(e) = store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .meta_set(&key, &digest)
                {
                    tracing::warn!("vault: saving what {peer} holds failed: {e:#}");
                }
                // The peer's own line ("stored a vault offer — merged …"),
                // not the summary's `state:` header.
                let said = reply.split("\n\n").nth(1).unwrap_or(&reply);
                tracing::info!("vault: pushed to {peer}: {}", said.trim());
            }
            Err(e) => {
                let wait = backoff.failed(&bkey, base, std::time::Instant::now());
                tracing::warn!(
                    "vault: auto-push to {peer} failed (next try in {}s): {e:#}",
                    wait.as_secs()
                );
            }
        }
    }
}

/// `[cluster] memsync_auto`: push this node's memory revisions newer
/// than each peer's cursor to every `memsync_peers` entry (the peer
/// applies on receipt when it has the flag too). The cursor — the
/// newest local revision when the push began — advances only when the
/// peer has stored the offer, so a peer that was down gets the backlog.
/// A revision written during the push is sent again next time, which the
/// receiver reads as unchanged. Revisions that came FROM a peer are
/// local revisions here too and travel on: memory reaches a node this
/// one is the only link to (the phone, through the VPS).
async fn memsync_push(
    store: &Arc<std::sync::Mutex<veles_core::store::Store>>,
    me: &veles_core::node::SelfNode,
    cfg: &veles_core::config::VelesConfig,
    base: std::time::Duration,
) {
    let backoff = veles_core::backoff::pool();
    let mem = veles_core::memory::MemoryStore::new();
    let high = match mem.latest_rev_id() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("memsync: reading the memory store failed: {e:#}");
            return;
        }
    };
    for peer in &cfg.cluster.memsync_peers {
        let key = veles_core::memsync::cursor_key(peer);
        let cursor: i64 = store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .meta_get(&key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if high <= cursor {
            continue;
        }
        let Some((url, bearer)) = peer_target(peer) else {
            tracing::warn!("cluster: memsync peer '{peer}' is not in [a2a_agents] — skipping");
            continue;
        };
        let bkey = format!("memsync:{peer}");
        if !backoff.ready(&bkey, std::time::Instant::now()) {
            continue;
        }
        match veles_core::memsync::push_to_peer_scoped(
            &mem,
            &me.identity.id,
            &me.name,
            &url,
            bearer.as_deref(),
            cursor,
            &cfg.cluster.local_memory_globs,
        )
        .await
        {
            Ok(line) => {
                backoff.succeeded(&bkey);
                if let Err(e) = store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .meta_set(&key, &high.to_string())
                {
                    tracing::warn!("memsync: saving {peer}'s cursor failed: {e:#}");
                }
                tracing::info!(
                    "memsync: pushed revisions {cursor}..{high} to {peer}: {}",
                    line.lines().next().unwrap_or_default()
                );
            }
            Err(e) => {
                let wait = backoff.failed(&bkey, base, std::time::Instant::now());
                tracing::warn!(
                    "memsync: auto-push to {peer} failed (next try in {}s): {e:#}",
                    wait.as_secs()
                )
            }
        }
    }
}

// ── token-plan quota warm (2026-09-15) ────────────────────────────

/// Warm the token-plan quota cache on a timer.
///
/// Exists so the readers never block: the desktop status field repaints
/// every 15s and `/quota` answers a chat line, and neither may pay a
/// round trip to Singapore. Everything reads
/// `tokenplan::cached`; this is the only thing in the process that
/// calls out on its own.
///
/// Announced rather than silent (the standing nothing-implicit rule):
/// the cadence — or the fact that there is none — is logged at start,
/// so "why is my quota figure an hour old" has an answer in the log.
async fn run_tokenplan_poller(cfg: std::sync::Arc<veles_core::config::VelesConfig>) {
    if !cfg.tokenplan.enabled {
        tracing::debug!("tokenplan: disabled, not polling");
        return;
    }
    let poll = cfg.tokenplan.poll_secs;
    if poll == 0 {
        tracing::info!("tokenplan: background warm disabled (poll_secs = 0)");
        return;
    }
    tracing::info!("tokenplan: warming the quota cache every {poll}s");
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(poll));
    // A missed tick must not turn into a burst of catch-up requests at
    // a paid API.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let snap = veles_core::tokenplan::refresh(&cfg, None).await;
        // Log only the failures, and only once per tick: a warm that
        // works is not news, and a warm that does not is the thing
        // somebody will be looking for.
        if let Some(e) = &snap.team_error {
            tracing::debug!("tokenplan: team warm failed: {e}");
        }
        if let Some(e) = &snap.personal_error {
            tracing::debug!("tokenplan: personal warm failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One store push per peer at a time, and the claim goes with the
    /// pass however it ends.
    #[test]
    fn a_push_in_flight_blocks_a_second_for_that_peer_only() {
        let a = PushInFlight::claim("test-peer-a").expect("free");
        assert!(
            PushInFlight::claim("test-peer-a").is_none(),
            "one at a time"
        );
        let b = PushInFlight::claim("test-peer-b").expect("another peer is free");
        drop(a);
        assert!(
            PushInFlight::claim("test-peer-a").is_some(),
            "released on drop"
        );
        drop(b);
    }

    /// A blocking task the runtime cancelled is the gateway stopping,
    /// not a failure to report — the phone's every idle stop used to
    /// log "refusal(s) pending" and "checking takeovers failed".
    #[tokio::test]
    async fn a_cancelled_task_reads_as_stopping() {
        let cancelled = || async {
            let h = veles_core::rt::spawn(std::future::pending::<()>());
            h.abort();
            anyhow::Error::from(h.await.unwrap_err())
        };
        assert!(stopping(&cancelled().await));
        assert!(stopping(&cancelled().await.context("checking takeovers")));
        assert!(!stopping(&anyhow::anyhow!("task 7 was cancelled")));
    }

    fn msg(platform: &str, chat: &str, id: Option<&str>, text: &str) -> InboundMessage {
        InboundMessage {
            platform: platform.into(),
            route: None,
            chat_id: chat.into(),
            msg_id: id.map(String::from),
            dedup_id: None,
            sender: "user".into(),
            // Test messages carry a verified identity so the legacy
            // default (user role) applies and turns aren't refused.
            sender_id: Some("irc:testuser".into()),
            sender_ctx: Default::default(),
            text: text.into(),
            context: None,
        }
    }

    /// A chat id no OTHER test shares, for tests that need their
    /// process-global progress window to themselves (the window is
    /// keyed by chat, and a concurrent `#chan` test's tail flush can
    /// consume another test's `⚙` line mid-assertion).
    fn private_chat() -> String {
        format!(
            "#p{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    /// End-to-end exactly-once through the REAL durable path
    /// (Store::open → ledger schema → claim_inbound). This is the path a
    /// redelivered telegram update takes after a gateway restart.
    #[test]
    fn redelivered_update_is_dropped_durably() {
        let dir = std::env::temp_dir().join(format!("veles-dedup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("state.db")).unwrap();

        let m = msg("telegram", "-1001", Some("501"), "hi");
        let key = dedup_key(&m);
        assert!(store.claim_inbound(&key).unwrap(), "first claim is fresh");
        assert!(!store.claim_inbound(&key).unwrap(), "redelivery is a dup");

        // A genuinely new message (same text, new id) passes.
        assert!(store
            .claim_inbound(&dedup_key(&msg("telegram", "-1001", Some("502"), "hi")))
            .unwrap());

        // Message ids restart per chat — 501 elsewhere is not a dup.
        assert!(store
            .claim_inbound(&dedup_key(&msg("telegram", "-1002", Some("501"), "other")))
            .unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn idless_platform_dedups_by_content() {
        let dir = std::env::temp_dir().join(format!("veles-dedup-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("state.db")).unwrap();

        let m = msg("ircx", "#chan", None, "ping");
        let key = dedup_key(&m);
        assert!(store.claim_inbound(&key).unwrap());
        assert!(!store.claim_inbound(&key).unwrap());

        // Different sender, same text: distinct key, not a dup.
        let mut m2 = msg("ircx", "#chan", None, "ping");
        m2.sender = "other".into();
        assert!(store.claim_inbound(&dedup_key(&m2)).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F2: the outbound choke point — the channel WRAPPER installed by
    /// `register_outbound`, which is what turn replies and steps send
    /// through — scrubs BOTH layers: registered secret values
    /// (`secrets.env` over `.env`) and credential SHAPES the map never
    /// knew about. Reasoning rides along when a surface shows it, so
    /// it is scrubbed too.
    #[tokio::test]
    async fn outbound_is_scrubbed_of_secrets_and_credential_shapes() {
        let (home, _env) = veles_core::testenv::scratch_home();
        std::fs::write(
            home.path().join("secrets.env"),
            "REG_KEY=registered-canary-abcdef\n",
        )
        .unwrap();

        let (tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let (atx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = GatewayBus {
            inbound: tx,
            outbound: Arc::new(Mutex::new(HashMap::new())),
            last_chat: Arc::new(Mutex::new(HashMap::new())),
            state_path: home.path().join("gateway-state.json"),
        };
        bus.register_outbound("irc", atx);

        // A compromised reply carrying the registered value AND an
        // unregistered key shape.
        let mut msg = OutboundMessage::plain(
            "#chan",
            "here: registered-canary-abcdef and sk-ant-abcdefghij1234567890 too",
            MessageKind::Reply,
        );
        msg.reasoning = Some("thinking about registered-canary-abcdef".into());
        assert!(bus.send_outbound("irc", msg));

        // The wrapper forwards asynchronously — poll for the delivery.
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("delivery within 2s")
            .expect("channel alive");
        assert!(
            got.text.contains("REG_KEY=<REDACTED>"),
            "registered values die: {}",
            got.text
        );
        assert!(
            got.text.contains("sk-ant-[redacted]"),
            "unregistered shapes die: {}",
            got.text
        );
        assert!(
            !got.text.contains("registered-canary-abcdef")
                && !got
                    .reasoning
                    .as_deref()
                    .unwrap_or("")
                    .contains("registered-canary-abcdef"),
            "no copy survives anywhere: {:?}",
            got
        );
        assert!(
            got.reasoning
                .as_deref()
                .is_some_and(|r| r.contains("REG_KEY=<REDACTED>")),
            "reasoning is scrubbed too: {:?}",
            got.reasoning
        );
    }

    /// Lifecycle notices route to the last known chat per platform and
    /// the last-chat map survives a bus rebuild (persistence).
    #[test]
    fn broadcast_notice_reaches_last_chat_and_persists() {
        let dir = std::env::temp_dir().join(format!("veles-bus-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("gateway-state.json");

        let (tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let (atx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = GatewayBus {
            inbound: tx,
            outbound: Arc::new(Mutex::new(HashMap::from([(
                "telegram".to_string(),
                HashMap::from([(String::new(), atx)]),
            )]))),
            last_chat: Arc::new(Mutex::new(HashMap::new())),
            state_path: state_path.clone(),
        };

        // A message from telegram chat 42 records it as the last chat.
        bus.send_inbound(msg("telegram", "42", Some("m1"), "hello"));
        assert_eq!(
            bus.last_chat.lock().unwrap().get("telegram").cloned(),
            Some("42".to_string())
        );

        // The notice goes out to that chat, not to an empty chat_id.
        let sent = bus.broadcast_notice("stopping");
        assert_eq!(sent, 1);
        let got = rx.try_recv().expect("notice should be delivered");
        assert_eq!(got.chat_id, "42");
        assert_eq!(got.text, "stopping");

        // Persistence: a rebuilt bus (restart) restores the last chat.
        let (tx2, _inbound_rx2) = tokio::sync::mpsc::unbounded_channel();
        let (atx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        let bus2 = GatewayBus {
            inbound: tx2,
            outbound: Arc::new(Mutex::new(HashMap::from([(
                "telegram".to_string(),
                HashMap::from([(String::new(), atx2)]),
            )]))),
            last_chat: Arc::new(Mutex::new(HashMap::new())),
            state_path,
        };
        if let Ok(raw) = std::fs::read_to_string(&bus2.state_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) {
                *bus2.last_chat.lock().unwrap() = map;
            }
        }
        assert_eq!(
            bus2.last_chat.lock().unwrap().get("telegram").cloned(),
            Some("42".to_string())
        );
        let sent2 = bus2.broadcast_notice("up");
        assert_eq!(sent2, 1);
        let got2 = rx2.try_recv().expect("boot notice should be delivered");
        assert_eq!(got2.chat_id, "42");
        assert_eq!(got2.text, "up");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two adapters of ONE platform are two audiences.
    ///
    /// The live failure (2026-09-11): `outbound` was
    /// `HashMap<platform, Sender>` and each IRC network called
    /// `register_outbound("irc", …)`, so the second to start REPLACED
    /// the first. Every reply on the platform then went out on that one
    /// connection, still addressed to whatever chat_id the turn came
    /// from — so the bot went mute on one network and, for any channel
    /// name that exists on both, answered a question from one room in a
    /// different room full of strangers.
    ///
    /// Mutation: drop the route from `register_outbound_route` (making
    /// both register under `""`) and the second registration wins both
    /// assertions.
    #[tokio::test]
    async fn two_networks_on_one_platform_do_not_steal_each_others_replies() {
        let dir = std::env::temp_dir().join(format!("veles-route-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = GatewayBus {
            inbound: tx,
            outbound: Arc::new(Mutex::new(HashMap::new())),
            last_chat: Arc::new(Mutex::new(HashMap::new())),
            state_path: dir.join("gateway-state.json"),
        };
        bus.register_outbound_route("irc", "irc.synirc.net", a_tx);
        bus.register_outbound_route("irc", "irc.IRC4Fun.net", b_tx);

        // A reply for synirc reaches synirc, and ONLY synirc.
        assert!(bus.send_outbound_to(
            "irc",
            Some("irc.synirc.net"),
            OutboundMessage::plain("#flashsupport", "hi", MessageKind::Reply),
        ));
        let got = a_rx.recv().await.expect("synirc gets its own reply");
        assert_eq!(got.chat_id, "#flashsupport");
        tokio::task::yield_now().await;
        assert!(
            b_rx.try_recv().is_err(),
            "the OTHER network must not see a reply addressed to a channel it is not in"
        );

        // …and the same channel NAME on the other network is a different
        // room. This is the case that makes the bug a leak rather than
        // an outage.
        assert!(bus.send_outbound_to(
            "irc",
            Some("irc.IRC4Fun.net"),
            OutboundMessage::plain("#coven", "there", MessageKind::Reply),
        ));
        assert_eq!(
            b_rx.recv().await.expect("irc4fun gets its own").text,
            "there"
        );
        tokio::task::yield_now().await;
        assert!(a_rx.try_recv().is_err(), "synirc must not see #coven");

        // An unknown route with several registered is refused, not
        // guessed: picking one would be picking an audience.
        assert!(
            !bus.send_outbound_to(
                "irc",
                Some("irc.libera.chat"),
                OutboundMessage::plain("#x", "nowhere", MessageKind::Reply),
            ),
            "no adapter matched, and any pick would be the wrong room"
        );

        // The boot notice reaches EVERY instance, each at its own last
        // chat — it used to fire once, for the platform, at whichever
        // chat the platform saw last.
        let sent = bus.broadcast_notice("up");
        assert_eq!(sent, 2, "one notice per connection, not per platform");
        assert_eq!(a_rx.recv().await.unwrap().chat_id, "#flashsupport");
        assert_eq!(b_rx.recv().await.unwrap().chat_id, "#coven");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single-adapter platform is untouched by any of the above.
    ///
    /// The fallbacks exist for exactly this: telegram registers on the
    /// default route and every route-unaware caller (cron, the plugin
    /// bridge) still reaches it, and a lone IRC network registered under
    /// its network name is still reachable by a caller naming no route.
    #[tokio::test]
    async fn one_adapter_is_reachable_however_the_caller_spells_the_route() {
        let dir = std::env::temp_dir().join(format!("veles-route1-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();

        // Default route: the shape telegram and a2a register with.
        let (t_tx, mut t_rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = GatewayBus {
            inbound: tx.clone(),
            outbound: Arc::new(Mutex::new(HashMap::new())),
            last_chat: Arc::new(Mutex::new(HashMap::new())),
            state_path: dir.join("a.json"),
        };
        bus.register_outbound("telegram", t_tx);
        assert!(bus.send_outbound(
            "telegram",
            OutboundMessage::plain("42", "a", MessageKind::Reply)
        ));
        assert_eq!(t_rx.recv().await.unwrap().text, "a");

        // One IRC network, registered under its name: a caller with no
        // route in hand (cron) must still land on it.
        let (i_tx, mut i_rx) = tokio::sync::mpsc::unbounded_channel();
        let bus2 = GatewayBus {
            inbound: tx,
            outbound: Arc::new(Mutex::new(HashMap::new())),
            last_chat: Arc::new(Mutex::new(HashMap::new())),
            state_path: dir.join("b.json"),
        };
        bus2.register_outbound_route("irc", "irc.synirc.net", i_tx);
        assert!(
            bus2.send_outbound("irc", OutboundMessage::plain("#c", "b", MessageKind::Reply)),
            "the only adapter is the unambiguous answer"
        );
        assert_eq!(i_rx.recv().await.unwrap().text, "b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── S15 ACL: gateway enforcement paths ──────────────────────────
    //
    // These drive the REAL `GatewayCore::handle` (banned drop, builtin
    // tiers, guest chat refusal, per-sender session split). All tested
    // paths return BEFORE any LLM call — zero tokens. The outbound
    // channel is observed directly, no platform adapter needed.

    /// Build a GatewayCore against a scratch home with a curated ACL:
    /// owner=irc:owner, roles: {irc:friend: trusted, irc:bad: banned}.
    /// Returns (core, rx, home, guard): rx observes ALL outbound
    /// messages, and the guard is the env lock — bind all four in the
    /// test body so the lock outlives every agent the core builds.
    fn acl_core() -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        acl_core_with(
            "owner: [\"irc:owner\"]\nroles: {\"irc:friend\": trusted, \"irc:bad\": banned}",
        )
    }

    /// The inbound-routing slot (R7): a layer installed by a module
    /// sees every message before core's routing and may swallow it;
    /// purging the module's owner puts core's routing back. The layer
    /// acts on ONE probe text only, so the other tests in this
    /// process, which share the slot, never meet it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_inbound_layer_sees_a_message_before_routing_and_a_purge_restores_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use veles_core::layers::{BoxFuture, InboundLayer, NextInbound};
        struct Swallow(Arc<AtomicUsize>);
        impl InboundLayer for Swallow {
            fn handle<'a>(
                &'a self,
                msg: InboundMessage,
                next: NextInbound<'a>,
            ) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    if msg.text == "layer-probe-drop" {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        return; // swallowed: core never routes it
                    }
                    next.handle(msg).await
                })
            }
        }
        let (core, mut rx, _tmp, _g) = acl_core();
        let seen = Arc::new(AtomicUsize::new(0));
        let prev = veles_core::lifecycle::thread_owner();
        veles_core::lifecycle::set_thread_owner("module:inbound-probe");
        veles_core::layers::install_inbound(Arc::new(Swallow(seen.clone())));
        veles_core::lifecycle::clear_thread_owner_for_test(prev);

        // A banned sender's message would be dropped by core with a
        // warning; the layer sees it FIRST and counts it.
        core.handle(acl_msg(Some("irc:bad"), "layer-probe-drop"))
            .await;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "the layer ran before routing"
        );
        assert!(
            rx.try_recv().is_err(),
            "a swallowed message produces no outbound reply"
        );
        // Another text passes through the layer to core's routing: the
        // banned sender is dropped there, silently, as without a layer.
        core.handle(acl_msg(Some("irc:bad"), "hello")).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        assert_eq!(
            veles_core::layers::purge_owner(Some("module:inbound-probe")),
            1
        );
        core.handle(acl_msg(Some("irc:bad"), "layer-probe-drop"))
            .await;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "after the purge the probe reaches core's routing, not the layer"
        );
    }

    /// `inbound.route` (PLAN-extension-api.md E3): an override rewrites a
    /// message's text or drops it; one that tries to change who sent it
    /// is refused and the ORIGINAL message is routed (the ACL resolves
    /// the sender after this point). The override acts on the chat
    /// `#pt-inbound` only, so the tests beside it pass through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_inbound_override_rewrites_drops_and_cannot_change_the_sender() {
        let (core, mut rx, _tmp, _g) = acl_core();
        let owner = veles_core::lifecycle::owner_tag("module:pt-inbound");
        veles_core::services::register_for(
            owner,
            veles_core::services::ServiceDecl {
                name: veles_core::points::INBOUND.into(),
                description: "a test's override".into(),
                cap: None,
            },
            veles_core::services::handler(|input, cx| async move {
                if input["chat_id"] != "#pt-inbound" {
                    return cx.next(input).await;
                }
                match (input["sender_id"].as_str(), input["text"].as_str()) {
                    (_, Some("pt-rewrite")) => {
                        let mut m = input.clone();
                        m["text"] = serde_json::json!("/acl");
                        Ok(serde_json::json!({"action": "route", "message": m}))
                    }
                    // The owner's /acl here is dropped: a routed /acl
                    // answers at once, so silence means it was dropped.
                    (Some("irc:owner"), Some("/acl")) => Ok(serde_json::json!({"action": "drop"})),
                    (Some("irc:friend"), Some("/acl")) => {
                        let mut m = input.clone();
                        m["sender_id"] = serde_json::json!("irc:owner");
                        Ok(serde_json::json!({"action": "route", "message": m}))
                    }
                    _ => cx.next(input).await,
                }
            }),
        )
        .unwrap();
        let msg = |sender: &str, text: &str| {
            let mut m = acl_msg(Some(sender), text);
            m.chat_id = "#pt-inbound".into();
            m
        };
        core.handle(msg("irc:owner", "pt-rewrite")).await;
        let owner_reply = rx.try_recv().expect("the rewritten /acl answers").text;
        assert!(owner_reply.contains("owner"), "{owner_reply}");
        core.handle(msg("irc:owner", "/acl")).await;
        assert!(rx.try_recv().is_err(), "a dropped message is never routed");
        core.handle(msg("irc:friend", "/acl")).await;
        let out = rx
            .try_recv()
            .expect("the friend's own /acl answers, not the owner's");
        assert!(out.text.contains("trusted"), "{}", out.text);
        assert_ne!(
            out.text, owner_reply,
            "the override's sender change was refused: the friend got its own report"
        );
        veles_core::services::purge_owner(Some(owner));
    }

    /// The same fixture over an arbitrary acl section, for the tests
    /// that are ABOUT the section rather than about what it permits.
    /// Both connectors have an outbound route registered, so a telegram
    /// test can observe silence the same way an IRC one observes a reply.
    fn acl_core_with(
        acl_yaml: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        // scratch_home, and the guard goes OUT in the tuple: the lock
        // must be held for the whole test body, not just the fixture
        // frame. While it was `let _g = lock()` in here, the fixture
        // published VELES_HOME and then RELEASED the lock on return, so
        // the test's agents — built on tokio workers that cannot see a
        // thread-local — read whatever home a neighbour published next
        // and cold-opened its fresh state.db mid-WAL-conversion:
        // "database is locked", once in a hundred runs (2026-09-28,
        // a_bind_fills… as the victim; instrumented 2026-10-01).
        let (tmp, guard) = veles_core::testenv::scratch_home();
        let dir = tmp.path().to_path_buf();

        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert(
            "irc".to_string(),
            HashMap::from([(String::new(), out_tx.clone())]),
        );
        outbound.insert(
            "telegram".to_string(),
            HashMap::from([(String::new(), out_tx.clone())]),
        );
        outbound.insert(
            "atproto".to_string(),
            HashMap::from([(String::new(), out_tx)]),
        );

        // Start from defaults, NOT config::load(): loading reads the
        // developer's real veles or ~/.hermes config, so this test
        // passed on a configured machine and failed anywhere else (it
        // fell over in the nix sandbox with "no model.provider in
        // config"). Everything the test needs is set explicitly below.
        let cfg = veles_core::config::VelesConfig {
            // Force the curated ACL regardless of the machine's own config.
            acl: serde_yaml::from_str(acl_yaml).unwrap(),
            // A route that resolves offline. No turn is ever run, but
            // building an Agent resolves the primary route eagerly.
            model: veles_core::config::ModelSection {
                provider: Some("test-provider".into()),
                default: Some("test-model".into()),
                ..Default::default()
            },
            custom_providers: vec![veles_core::config::ProviderEntry {
                name: "test-provider".into(),
                base_url: "http://127.0.0.1:0/v1".into(),
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        let hooks = HookBus::new(None); // no Tcl in tests — already Arc
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks,
        });
        (core, out_rx, tmp, guard)
    }

    fn acl_msg(sender_id: Option<&str>, text: &str) -> InboundMessage {
        InboundMessage {
            platform: "irc".into(),
            route: None,
            chat_id: "#chan".into(),
            msg_id: Some(format!(
                "m{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
            dedup_id: None,
            sender: "someone".into(),
            sender_id: sender_id.map(String::from),
            sender_ctx: Default::default(),
            text: text.into(),
            context: None,
        }
    }

    /// A Bluesky mention from a mutual, threaded under `root`.
    fn feed_msg(sender: &str, text: &str) -> InboundMessage {
        let mut m = acl_msg(Some(sender), text);
        m.platform = "atproto".into();
        m.chat_id = "at://did:plc:op/app.bsky.feed.post/root".into();
        // Unique per call (the inbound ledger drops a repeated id).
        m.msg_id = Some(format!(
            "at://did:plc:mutual/app.bsky.feed.post/{}#bafy",
            m.msg_id.as_deref().unwrap_or_default()
        ));
        m
    }

    /// On a public feed every answer is a public post: a mutual's
    /// `/nodes` or `/status` would publish the pool. Only the public
    /// commands pass; the owner keeps everything (USER 2026-10-06). The
    /// refusal, like every command answer, threads under the mention.
    #[tokio::test]
    async fn a_public_feed_gives_a_mutual_only_the_public_commands() {
        let (core, mut rx, _tmp, _g) = acl_core_with(
            "owner: [\"atproto:did:plc:op\"]\nroles: {\"atproto:did:plc:mutual\": user}",
        );
        for cmd in ["/nodes", "/status", "/usage", "/acl", "/pair new"] {
            let m = feed_msg("atproto:did:plc:mutual", cmd);
            let mention = m.msg_id.clone();
            core.handle(m).await;
            let out = rx.try_recv().unwrap_or_else(|_| panic!("{cmd}: an answer"));
            assert!(
                out.text.contains("not available here"),
                "{cmd} must be refused on a public feed: {}",
                out.text
            );
            assert_eq!(
                out.reply_to, mention,
                "{cmd}: the answer threads under the mention, not the root"
            );
        }
        core.handle(feed_msg("atproto:did:plc:mutual", "/help"))
            .await;
        let help = rx.try_recv().expect("help answers");
        assert_eq!(
            help.text, PUBLIC_FEED_HELP,
            "help lists only what the feed allows"
        );
        // The owner is not limited.
        core.handle(feed_msg("atproto:did:plc:op", "/status")).await;
        let status = rx.try_recv().expect("the owner's status");
        assert!(
            !status.text.contains("not available here"),
            "the owner keeps every command: {}",
            status.text
        );
    }

    /// A public `/tcl` is the public-reply POLICY's word (the slopdrop
    /// module's, R5). With none loaded the feed gets a fixed note and
    /// nothing of the output — not even an evaluation; with one, the
    /// policy's text and `meta` ride the outbound message.
    struct Shaper;
    impl veles_core::tcleval::PublicTclReply for Shaper {
        fn shape<'a>(
            &'a self,
            req: veles_core::tcleval::PublicReplyRequest<'a>,
        ) -> veles_core::tcleval::ShapeFuture<'a> {
            Box::pin(async move {
                let mut meta = std::collections::BTreeMap::new();
                meta.insert(
                    veles_core::bus::meta_keys::LEDGER_ENTRY.to_string(),
                    format!("entry-for-{}", req.code),
                );
                veles_core::tcleval::PublicReplyShaped {
                    text: format!("shaped:{}", req.output),
                    meta,
                }
            })
        }
    }

    #[tokio::test]
    async fn a_public_tcl_is_the_policys_word_and_fails_closed_without_one() {
        let (core, mut rx, _tmp, _g) = acl_core_with("owner: [\"atproto:did:plc:op\"]");
        veles_core::tcleval::clear_public_reply();
        core.handle(feed_msg("atproto:did:plc:op", "/tcl puts 42"))
            .await;
        let first = rx.try_recv().expect("a reply");
        assert_eq!(
            first.text,
            veles_core::tcleval::PUBLIC_REPLY_UNAVAILABLE,
            "no policy: a fixed note, never the output"
        );
        assert!(first.meta.is_empty());

        veles_core::tcleval::set_public_reply(std::sync::Arc::new(Shaper));
        core.handle(feed_msg("atproto:did:plc:op", "/tcl puts 42"))
            .await;
        let second = rx.try_recv().expect("a reply");
        veles_core::tcleval::clear_public_reply();
        assert!(
            second.text.starts_with("shaped:") && second.text.contains("42"),
            "the policy shaped the evaluated output: {}",
            second.text
        );
        assert_eq!(
            second
                .meta
                .get(veles_core::bus::meta_keys::LEDGER_ENTRY)
                .map(String::as_str),
            Some("entry-for-puts 42")
        );
    }

    #[test]
    fn the_public_feed_rule_passes_the_public_commands_and_other_platforms() {
        let user =
            veles_core::acl::Principal::known("atproto:did:plc:m", veles_core::acl::Role::User);
        for ok in [
            "/tcl expr 1",
            "/eval x",
            "/gen a cat",
            "/help",
            "hello there",
        ] {
            assert!(public_feed_refusal("atproto", ok, &user).is_none(), "{ok}");
        }
        assert!(public_feed_refusal("atproto", "/nodes", &user).is_some());
        // A chat surface is not a feed: nothing changes there.
        assert!(public_feed_refusal("irc", "/nodes", &user).is_none());
        assert!(public_feed_refusal("telegram", "/acl", &user).is_none());
    }

    /// Lockdown stops a connector at the gateway door: no turn, no
    /// command, one notice, then silence. And `/lockdown` itself still
    /// answers, because a sender who was just refused must be able to
    /// find out what refused them.
    #[tokio::test]
    async fn lockdown_refuses_connectors_once_and_explains_itself() {
        let (core, mut rx, _home, _guard) = acl_core();
        veles_core::lockdown::engage(&core.store, "tui:owner", "an incident").unwrap();

        // The owner over IRC is still a connector.
        core.handle(acl_msg(Some("irc:owner"), "hello?")).await;
        let first = rx.try_recv().expect("the first message is answered");
        assert!(
            first.text.contains("locked down"),
            "the refusal names the state: {}",
            first.text
        );
        assert!(
            first.text.contains("local frontend"),
            "…and where the way out is: {}",
            first.text
        );

        // Second line from the same sender: silence.
        core.handle(acl_msg(Some("irc:owner"), "still there?"))
            .await;
        assert!(
            rx.try_recv().is_err(),
            "a locked-down bot must not answer every line"
        );

        // A DIFFERENT sender is told once too.
        let mut other = acl_msg(Some("irc:friend"), "hey");
        other.sender = "friend".into();
        core.handle(other).await;
        assert!(
            rx.try_recv().is_ok(),
            "each sender hears it once, not the first one only"
        );

        // …but `/lockdown` is answered even while engaged.
        core.handle(acl_msg(Some("irc:owner"), "/lockdown")).await;
        let status = rx.try_recv().expect("/lockdown answers under lockdown");
        assert!(
            status.text.contains("an incident"),
            "the status names the reason it was engaged: {}",
            status.text
        );

        // And `off` from a connector is refused, with the reason.
        core.handle(acl_msg(Some("irc:owner"), "/lockdown off"))
            .await;
        let refused = rx.try_recv().expect("an answer, not silence");
        assert!(
            refused.text.contains("LOCAL"),
            "the asymmetry is explained rather than just enforced: {}",
            refused.text
        );
        assert!(
            veles_core::lockdown::engaged(Some(&core.store)),
            "a connector must not be able to lift it"
        );
        veles_core::lockdown::release(&core.store).unwrap();
    }

    // ── the door: [acl.strangers] ───────────────────────────────────
    //
    // USER, 2026-09-16, about the telegram bot: *"That channel should
    // only really resolve me as owner at the moment, I want to add any
    // other people I want to speak to it explicitly and ignore everyone
    // else."* `default_role` could not express that — it answers what an
    // unlisted sender may DO, and every answer it has still includes a
    // billed turn, because Role::Guest has Cap::Chat by design.

    fn tg_msg(sender_id: Option<&str>, text: &str) -> InboundMessage {
        let mut m = acl_msg(sender_id, text);
        m.platform = "telegram".into();
        m.chat_id = "123456789".into();
        m.sender_ctx = veles_core::acl::SenderContext {
            platform: Some("telegram".into()),
            chat_id: Some("123456789".into()),
            direct: true,
            ..Default::default()
        };
        m
    }

    /// The acl section this box now runs: one named human, and a door
    /// shut against everyone the file does not name.
    const OWNER_ONLY_TELEGRAM: &str = "default_role: guest\n\
         strangers: {telegram: ignore}\n\
         users:\n\
         \x20 - name: wrath\n\
         \x20   role: owner\n\
         \x20   identities: [\"telegram:123456789\", \"irc:xenia\"]\n\
         roles: {\"telegram:42\": user}";

    #[tokio::test]
    async fn a_telegram_stranger_is_ignored_at_the_door_not_admitted_as_a_guest() {
        let (core, mut rx, _home, _guard) = acl_core_with(OWNER_ONLY_TELEGRAM);

        // Without the door they would resolve as a guest — and a guest
        // may chat, which is a turn, which is money.
        let would_be = core.resolve(Some("telegram:111222333"), &Default::default());
        assert_eq!(
            would_be.role,
            veles_core::acl::Role::Guest,
            "the ACL itself still says guest; the door is a separate question"
        );
        assert!(would_be.has(veles_core::acl::Cap::Chat));

        // `/status` is the guest floor: it answers for any admitted
        // sender, so a silent one proves the message never got in
        // rather than proving the command was refused.
        core.handle(tg_msg(Some("telegram:111222333"), "/status"))
            .await;
        assert!(
            rx.try_recv().is_err(),
            "a sender no [acl] rule names must get nothing on this connector"
        );
    }

    #[tokio::test]
    async fn the_owner_and_anyone_named_still_get_through_that_door() {
        let (core, mut rx, _home, _guard) = acl_core_with(OWNER_ONLY_TELEGRAM);

        core.handle(tg_msg(Some("telegram:123456789"), "/status"))
            .await;
        let out = rx
            .try_recv()
            .expect("the owner is named, so the owner is let in");
        assert!(out.text.contains("veles gateway"), "{}", out.text);

        // "anyone else I want to speak to it, explicitly" — one line in
        // the same file, and nothing else about the door changes.
        core.handle(tg_msg(Some("telegram:42"), "/status")).await;
        let out = rx
            .try_recv()
            .expect("an explicitly named guest is not a stranger");
        assert!(out.text.contains("veles gateway"), "{}", out.text);
    }

    #[tokio::test]
    async fn the_door_is_per_connector_and_shuts_nothing_else() {
        let (core, mut rx, _home, _guard) = acl_core_with(OWNER_ONLY_TELEGRAM);
        // The same unnamed sender over IRC, where no policy was written.
        core.handle(acl_msg(Some("irc:stranger"), "/status")).await;
        let out = rx
            .try_recv()
            .expect("shutting the telegram door must not shut IRC's");
        assert!(out.text.contains("veles gateway"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_connector_pinned_to_a_role_uses_it_instead_of_default_role() {
        // The other half of the knob: not "nobody", but "strangers here
        // are X" — the shape an operator reaches for when a connector is
        // more trusted than the file's default, or less.
        let (core, _rx, _home, _guard) = acl_core_with(
            "default_role: guest\nstrangers: {telegram: banned}\nowner: [\"irc:owner\"]",
        );
        let p = core.resolve(
            Some("telegram:111222333"),
            &veles_core::acl::SenderContext {
                platform: Some("telegram".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            p.role,
            veles_core::acl::Role::Banned,
            "the connector decides"
        );
        let elsewhere = core.resolve(Some("irc:nobody"), &Default::default());
        assert_eq!(
            elsewhere.role,
            veles_core::acl::Role::Guest,
            "and only this connector"
        );
    }

    #[tokio::test]
    async fn acl_banned_sender_is_dropped_silently() {
        let (core, mut rx, _home, _guard) = acl_core();
        core.handle(acl_msg(Some("irc:bad"), "hello?")).await;
        // Banned: NO outbound at all (not even a refusal — silence).
        assert!(rx.try_recv().is_err(), "banned sender must get nothing");
    }

    #[tokio::test]
    async fn acl_guest_can_chat_but_only_reads_and_only_in_a_box() {
        let (core, mut rx, _home, _guard) = acl_core();
        // Unknown verified sender in curated mode = guest, and a guest
        // may TALK since 2026-09-09 (USER: "Guests should have
        // read-only access and only inside of the sandbox" — which
        // presumes a conversation, and which is why they have a context
        // ceiling of their own). What they may not do is change
        // anything.
        let guest = core.resolve(Some("irc:stranger"), &Default::default());
        assert_eq!(guest.role, veles_core::acl::Role::Guest);
        assert!(guest.has(veles_core::acl::Cap::Chat), "a guest may talk");
        assert!(
            guest.has(veles_core::acl::Cap::FileRead),
            "a guest may read — inside their box, which is where the \
             zone routing sends every below-admin call"
        );
        for denied in [
            veles_core::acl::Cap::FileWrite,
            veles_core::acl::Cap::Shell,
            veles_core::acl::Cap::Web,
            veles_core::acl::Cap::MemoryRead,
        ] {
            assert!(
                !guest.has(denied),
                "a guest must not have {denied:?} — read-only means read-only"
            );
        }

        // Guest CAN use the status floor...
        core.handle(acl_msg(Some("irc:stranger"), "/status")).await;
        let out = rx.try_recv().expect("guest gets /status");
        assert!(
            out.text.contains("veles gateway"),
            "guest /status must answer: {}",
            out.text
        );

        // ...and bare /acl too, because that one is ABOUT them: a
        // caller who is being refused things is entitled to know what
        // they are and what their ceiling is. It used to be denied at
        // cmd-admin, which meant the person most in need of the answer
        // was the one who could not get it.
        core.handle(acl_msg(Some("irc:stranger"), "/acl")).await;
        let out = rx.try_recv().expect("guest gets their own /acl report");
        assert!(
            out.text.contains("guest") && out.text.contains("caps:"),
            "bare /acl names the caller's own role and caps: {}",
            out.text
        );
        // The RULES are a different question, and that one is gated.
        core.handle(acl_msg(Some("irc:stranger"), "/acl list"))
            .await;
        let out = rx.try_recv().expect("guest gets an /acl list denial");
        assert!(
            out.text.contains("[acl]") && out.text.contains("cmd-admin"),
            "/acl list must be denied for guests, naming the cap: {}",
            out.text
        );

        // A sender with no account and no mask cannot be told apart
        // from anyone else, so they are a guest too — and get the same
        // read-only floor rather than silence.
        let anon = core.resolve(None, &Default::default());
        assert_eq!(anon.role, veles_core::acl::Role::Guest);
        assert!(anon.subject.starts_with("anon:"), "{}", anon.subject);
    }

    #[tokio::test]
    async fn acl_owner_and_friend_pass_and_sessions_split() {
        let (core, mut rx, _home, _guard) = acl_core();
        // Owner: /acl works, shows owner role.
        core.handle(acl_msg(Some("irc:owner"), "/acl")).await;
        let out = rx.try_recv().expect("owner /acl answers");
        assert!(
            out.text.contains("owner"),
            "owner /acl shows role: {}",
            out.text
        );

        // Friend (trusted): bare /acl reports on THEM, and the tier
        // shows up on the subcommand that reads the rules.
        core.handle(acl_msg(Some("irc:friend"), "/acl")).await;
        let out = rx.try_recv().expect("friend gets their own /acl report");
        assert!(
            out.text.contains("trusted"),
            "bare /acl names the caller's role: {}",
            out.text
        );
        core.handle(acl_msg(Some("irc:friend"), "/acl grant x user"))
            .await;
        let out = rx.try_recv().expect("friend gets the grant denial");
        assert!(
            out.text.contains("[acl]") && out.text.contains("all"),
            "trusted must not grant, and the denial names the cap: {}",
            out.text
        );

        // Session split: friend and owner in the SAME chat have distinct
        // session keys (owner collapses to chat key, friend doesn't).
        let owner_key = "irc:#chan".to_string();
        let friend_key = "irc:#chan:irc:friend".to_string();
        // (No turn is run — creating the agent would call the LLM. The
        // key logic is exercised via agent_for_sender's session map; we
        // create both agents but never run a turn.)
        let _a = core
            .agent_for_sender("irc", "#chan", Some("irc:owner"), &Default::default())
            .unwrap();
        let _b = core
            .agent_for_sender("irc", "#chan", Some("irc:friend"), &Default::default())
            .unwrap();
        // Scoped: the std guard must be dropped before the tokio
        // lock().await below, or the await holds it across a yield.
        let friend_agent = {
            let sessions = core.sessions.lock().unwrap();
            assert!(
                sessions.contains_key(&owner_key),
                "owner uses the chat-level key"
            );
            assert!(
                sessions.contains_key(&friend_key),
                "friend gets a per-sender key"
            );
            assert_eq!(sessions.len(), 2);
            // And the friend's agent has a principal with memory-read…
            sessions.get(&friend_key).unwrap().clone()
        };
        let caps = friend_agent.principal().unwrap();
        assert!(caps.has(veles_core::acl::Cap::MemoryRead));
        assert!(!caps.has(veles_core::acl::Cap::MemoryWrite));
        // …and its frozen prompt was built WITHOUT memory injection is
        // impossible to assert here (memory files may exist); the prompt
        // tests in prompt.rs cover that directly.
    }
    // ── plugin gateway filters, through the GATEWAY ──────────────────
    //
    // Both filter paths existed and neither was tested here. The Tcl
    // one was checked by calling `alias_filter::handle` directly and
    // the Rust one by calling `run_gateway_filters` directly — which
    // proves a handler returns a verdict, and says nothing about
    // whether the gateway consumes it. The skip/rewrite plumbing in
    // `handle` was unexercised.
    //
    // A rewrite is only observable if the turn actually runs, so these
    // point the route at a local fixture and assert on the request the
    // MODEL received. That is the only place the rewritten text shows
    // up.

    /// A chat-completions endpoint that captures request bodies.
    async fn capturing_llm() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = Arc::clone(&seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        veles_core::rt::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut raw = Vec::new();
                let mut buf = [0u8; 8192];
                let mut need: Option<usize> = None;
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                    if need.is_none() {
                        if let Some(end) =
                            raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
                        {
                            let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            need = Some(end + len);
                        }
                    }
                    if need.is_some_and(|n| raw.len() >= n) {
                        break;
                    }
                }
                cap.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).to_string());
                // A STREAMING answer, as the turn asks for one: a plain JSON
                // body to a `stream: true` request carries no events, and
                // the reader used to take that as a finished empty answer —
                // the silent-empty-turn bug (now a retried cut).
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"pong\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
                    "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
                    "data: [DONE]\n\n"
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    /// `acl_core`, but with a chosen hook bus and a reachable route.
    /// The guard rides out in the tuple — see `acl_core_with` for why
    /// the fixture frame is not enough.
    fn filter_core(
        hooks: Arc<HookBus>,
        base_url: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        let (tmp, guard) = veles_core::testenv::scratch_home();
        let (core, rx) = filter_core_at(tmp.path(), hooks, base_url);
        (core, rx, tmp, guard)
    }

    /// The core of `filter_core` at a directory the CALLER owns — for
    /// tests that already hold a scratch home (the Tcl plugin test) or
    /// build two cores (the progress-budget test, where a second
    /// scratch_home would deadlock on the non-reentrant env lock).
    fn filter_core_at(
        dir: &std::path::Path,
        hooks: Arc<HookBus>,
        base_url: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
    ) {
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert("irc".to_string(), HashMap::from([(String::new(), out_tx)]));
        let cfg = veles_core::config::VelesConfig {
            model: veles_core::config::ModelSection {
                provider: Some("fixture".into()),
                default: Some("fixture-model".into()),
                ..Default::default()
            },
            custom_providers: vec![veles_core::config::ProviderEntry {
                name: "fixture".into(),
                base_url: base_url.to_string(),
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks,
        });
        (core, out_rx)
    }

    /// `filter_core`, but with a chosen `[acl]` section — so a test can
    /// set a budget of one and watch the second turn be refused.
    fn budget_core(
        base_url: &str,
        acl_yaml: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        budget_core_with(
            base_url,
            acl_yaml,
            veles_core::config::InjectionsSection::default(),
        )
    }

    /// `budget_core` at a directory the CALLER owns — see
    /// `filter_core_at`.
    fn budget_core_at(
        dir: &std::path::Path,
        base_url: &str,
        acl_yaml: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
    ) {
        budget_core_with_at(
            dir,
            base_url,
            acl_yaml,
            veles_core::config::InjectionsSection::default(),
        )
    }

    /// `budget_core`, with a chosen injections snapshot — for the
    /// tests about whose wake is billed and whose is not. The guard
    /// rides out in the tuple — see `acl_core_with`.
    fn budget_core_with(
        base_url: &str,
        acl_yaml: &str,
        injections: veles_core::config::InjectionsSection,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        let (tmp, guard) = veles_core::testenv::scratch_home();
        let (core, rx) = budget_core_with_at(tmp.path(), base_url, acl_yaml, injections);
        (core, rx, tmp, guard)
    }

    fn budget_core_with_at(
        dir: &std::path::Path,
        base_url: &str,
        acl_yaml: &str,
        injections: veles_core::config::InjectionsSection,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
    ) {
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert("irc".to_string(), HashMap::from([(String::new(), out_tx)]));
        let cfg = veles_core::config::VelesConfig {
            acl: serde_yaml::from_str(acl_yaml).unwrap(),
            model: veles_core::config::ModelSection {
                provider: Some("fixture".into()),
                default: Some("fixture-model".into()),
                ..Default::default()
            },
            custom_providers: vec![veles_core::config::ProviderEntry {
                name: "fixture".into(),
                base_url: base_url.to_string(),
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            agent: veles_core::config::AgentSection {
                injections,
                ..Default::default()
            },
            ..Default::default()
        };
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks: HookBus::new(None),
        });
        (core, out_rx)
    }

    /// The next REPLY, skipping the Progress traffic a real turn emits
    /// (typing indicator, streamed steps). Tests that assert on reply
    /// TEXT have to skip those or they assert on an empty string, which
    /// passes every `!contains` check for the wrong reason.
    fn next_reply(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
    ) -> Option<OutboundMessage> {
        while let Ok(m) = rx.try_recv() {
            if m.kind == MessageKind::Reply {
                return Some(m);
            }
        }
        None
    }

    /// A flood must cost NOTHING, not cost less.
    ///
    /// The turn-rate brake is the only one that fires before the
    /// provider is reached: the token ceiling needs a measurement to
    /// compare against, so by the time it can answer, the calls it
    /// measured are already paid for. Asserted on the captured request
    /// log, because "was the model called" is the whole claim.
    ///
    /// It also must not bill COMMANDS as turns — `/status` costs no
    /// provider call and must not consume a budget.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_flood_is_refused_before_the_provider_and_commands_are_not_billed() {
        let (url, seen) = capturing_llm().await;
        let (core, mut rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\n\
             roles: {\"irc:chatty\": user}\n\
             limits:\n\
             \x20 turns_per_hour_by_role: {user: 2}\n",
        );

        // Commands first: none of these may consume the budget of 2.
        //
        // `/tcl` is in the list because it is the one the USER asked
        // for by name (2026-09-11: "make sure that .tcl|/tcl bypass the
        // llm so that they don't consume a turn for a simple request
        // from a dumb machine"). It used to have no tier and no arm, so
        // it fell through `send_inbound` to the model as a literal
        // prompt — a billed provider call to compute `expr {1+1}`. The
        // budget assertions below are what prove it is answered instead
        // of forwarded.
        for cmd in ["/status", "/limits", "/tcl expr {40 + 2}"] {
            core.handle(acl_msg(Some("irc:chatty"), cmd)).await;
            let _ = rx.try_recv();
        }
        assert!(
            seen.lock().unwrap().is_empty(),
            "no command may reach the provider — /tcl least of all"
        );

        for i in 0..2 {
            core.handle(acl_msg(Some("irc:chatty"), "hello")).await;
            let out = next_reply(&mut rx).expect("a turn answers");
            assert!(
                !out.text.contains("ceiling for user"),
                "turn {i} should be inside the budget — commands were billed: {}",
                out.text
            );
        }
        let calls_before = seen.lock().unwrap().len();
        assert!(calls_before > 0, "the allowed turns did reach the model");

        // The third is refused, and says how long and how to raise it.
        core.handle(acl_msg(Some("irc:chatty"), "hello")).await;
        let out = next_reply(&mut rx).expect("the refusal is delivered");
        assert!(
            out.text.contains("ceiling for user") && out.text.contains("/limits turns-per-hour"),
            "the refusal must name the ceiling and the knob: {}",
            out.text
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            calls_before,
            "a refused turn must not reach the model at all"
        );

        // The OWNER in the same gateway is not on a rate at all.
        for _ in 0..5 {
            core.handle(acl_msg(Some("irc:owner"), "hello")).await;
            let out = next_reply(&mut rx).expect("owner turns answer");
            assert!(
                !out.text.contains("ceiling"),
                "owners are not rate limited: {}",
                out.text
            );
        }
    }

    /// One person in six channels takes six chat locks and none of them
    /// sees the others — which is why the concurrency brake is keyed on
    /// the SUBJECT and not on the chat.
    ///
    /// The permit itself is asserted rather than two genuinely in-flight
    /// turns: `gated_llm` below is the timing fixture that makes the
    /// end-to-end overlap testable without a race, and
    /// `a_message_parked_behind_a_long_turn…` uses it. What belongs here
    /// is the unit answer — that the permit refuses, refuses with the
    /// limit, and frees on drop.
    #[test]
    fn concurrent_turns_are_capped_per_subject_and_freed_on_drop() {
        let (core, _rx, _home, _guard) = budget_core(
            "http://127.0.0.1:0/v1",
            "owner: [\"irc:owner\"]\n\
             roles: {\"irc:eager\": user}\n\
             limits:\n\
             \x20 max_concurrent_turns: 1\n",
        );
        let held = core
            .turn_permit("irc:eager", 1)
            .expect("the first turn is admitted");
        assert!(held.is_some(), "a limit of 1 hands out a real permit");
        assert_eq!(
            core.turn_permit("irc:eager", 1).unwrap_err(),
            1,
            "the second is refused, and the error carries the limit for the message"
        );
        // Somebody else's turns are their own business.
        assert!(core.turn_permit("irc:someone-else", 1).is_ok());
        // Finishing frees the slot — the brake must not be a one-shot.
        drop(held);
        assert!(
            core.turn_permit("irc:eager", 1).is_ok(),
            "a finished turn must release its permit"
        );

        // 0 = unbounded, which is what admins and owners resolve to.
        for _ in 0..20 {
            assert!(matches!(core.turn_permit("irc:boss", 0), Ok(None)));
        }
        let acl: veles_core::acl::AclConfig =
            serde_yaml::from_str("owner: [\"irc:owner\"]\nlimits:\n  max_concurrent_turns: 1\n")
                .unwrap();
        assert_eq!(
            acl.limits
                .concurrent_turns_for_role(veles_core::acl::Role::Owner),
            0
        );
        assert_eq!(
            acl.limits
                .concurrent_turns_for_role(veles_core::acl::Role::User),
            1
        );
    }

    /// A limit lowered under turns that are ALREADY running takes effect
    /// on the next admission.
    ///
    /// The semaphore this replaced was rebuilt when the limit changed, so
    /// the permits the running turns held belonged to an object nobody
    /// counted any more: lowering `max_concurrent_turns` from 2 to 1 with
    /// two turns in flight admitted a third beside them. Turning the knob
    /// is something an operator does WHILE looking at a flood, which is
    /// exactly when that answer is wrong.
    #[test]
    fn lowering_the_turn_limit_counts_the_turns_already_running() {
        let (core, _rx, _home, _guard) = budget_core(
            "http://127.0.0.1:0/v1",
            "owner: [\"irc:owner\"]\nroles: {\"irc:eager\": user}\n",
        );
        let first = core
            .turn_permit("irc:eager", 2)
            .expect("admitted")
            .expect("a permit");
        let second = core
            .turn_permit("irc:eager", 2)
            .expect("admitted")
            .expect("a permit");
        assert_eq!(
            core.turn_permit("irc:eager", 2).unwrap_err(),
            2,
            "two turns are running at a limit of 2"
        );

        // The knob drops to 1 while BOTH are still holding theirs.
        assert_eq!(
            core.turn_permit("irc:eager", 1).unwrap_err(),
            1,
            "two turns are in flight; a limit of 1 must refuse the next one rather than rebuild around them"
        );

        // Freeing one leaves the subject AT the new limit, not under it —
        // the count is the truth, not the limit a permit was issued under.
        drop(second);
        assert_eq!(
            core.turn_permit("irc:eager", 1).unwrap_err(),
            1,
            "one turn is still running and the limit is 1"
        );
        drop(first);
        assert!(
            core.turn_permit("irc:eager", 1)
                .expect("admitted")
                .is_some(),
            "…and with both finished it admits"
        );

        // Raising the limit live admits at once, with no rebuild and no
        // restart: the same read-at-admit-time that made lowering honest.
        let held = core
            .turn_permit("irc:eager", 3)
            .expect("admitted")
            .expect("a permit");
        assert!(
            core.turn_permit("irc:eager", 9)
                .expect("admitted")
                .is_some(),
            "a raised limit admits immediately"
        );
        drop(held);
        // And the guard's decrement is what makes any of the above
        // reusable: a slot that never frees is a subject silently banned.
        assert!(
            core.turn_permit("irc:eager", 1)
                .expect("admitted")
                .is_some(),
            "every dropped guard must have freed its slot"
        );
    }

    /// A provider that holds its FIRST request open until the test says
    /// so, and answers every later one at once.
    ///
    /// The timing fixture `concurrent_turns_are_capped_per_subject…`
    /// declined to build, on the grounds that a flaky test is worse than a
    /// narrow one. A barrier rather than a sleep is what makes this one
    /// not flaky: nothing races, because the caller waits for the request
    /// to ARRIVE before sending the second message, and releases the
    /// barrier only once that message is parked behind it.
    ///
    /// Returns the base URL, the release gate, "a request is parked", and
    /// the request log.
    async fn gated_llm() -> (
        String,
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let gate = Arc::new(tokio::sync::Notify::new());
        let parked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (g, p, s) = (Arc::clone(&gate), Arc::clone(&parked), Arc::clone(&seen));
        veles_core::rt::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (g, p, s) = (Arc::clone(&g), Arc::clone(&p), Arc::clone(&s));
                veles_core::rt::spawn(async move {
                    // Read the whole request before deciding anything: a
                    // client writing into a socket nobody drains would be
                    // a second hang, and an unintended one.
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 8192];
                    let mut need: Option<usize> = None;
                    loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&buf[..n]);
                        if need.is_none() {
                            if let Some(end) =
                                raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
                            {
                                let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                                let len = head
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                need = Some(end + len);
                            }
                        }
                        if need.is_some_and(|n| raw.len() >= n) {
                            break;
                        }
                    }
                    let first = {
                        let mut log = s.lock().unwrap();
                        log.push(String::from_utf8_lossy(&raw).to_string());
                        log.len() == 1
                    };
                    if first {
                        p.store(true, std::sync::atomic::Ordering::SeqCst);
                        g.notified().await;
                        p.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    // SSE, not a bare JSON body: the request asks for a
                    // stream, and a non-stream answer parses to an empty
                    // reply — which would make "the long turn answered"
                    // untestable and the fixture prove less than it says.
                    let chunk = r#"{"choices":[{"delta":{"content":"pong"}}]}"#;
                    let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}/v1"), gate, parked, seen)
    }

    /// A message that arrives DURING a turn parks on the chat lock and
    /// claims its OWN turn when that lock drains — so it can be refused by
    /// a brake that was not tripped when it arrived.
    ///
    /// Pinned rather than changed, and the pin is the point. Claiming at
    /// ARRIVAL would bill messages that never reach a turn: the claim sits
    /// below the command branches and the dedup gate on purpose, so
    /// `/status` and a redelivered update cost nothing. Exempting a parked
    /// message would be worse — one long turn would carry N-1 free turns
    /// behind it, which is the flood the brake exists to stop.
    ///
    /// What the behaviour costs is a surprise worth writing down, and now
    /// a test: the refusal a caller reads can name a ceiling they were
    /// UNDER when they spoke, because the hour's budget was spent by the
    /// turn they were queued behind.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_message_parked_behind_a_long_turn_claims_its_turn_when_it_drains() {
        let (url, gate, parked, seen) = gated_llm().await;
        let (core, mut rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\n\
             roles: {\"irc:chatty\": user}\n\
             limits:\n\
             \x20 turns_per_hour_by_role: {user: 1}\n",
        );

        // Turn one takes the hour's only token, then parks on the
        // provider with the chat lock held.
        let c1 = Arc::clone(&core);
        let first =
            veles_core::rt::spawn(
                async move { c1.handle(acl_msg(Some("irc:chatty"), "one")).await },
            );
        for _ in 0..400 {
            if parked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            parked.load(std::sync::atomic::Ordering::SeqCst),
            "the long turn never reached the provider, so nothing is being proved"
        );

        // Turn two arrives while the lock is held, and parks behind it.
        let c2 = Arc::clone(&core);
        let second =
            veles_core::rt::spawn(
                async move { c2.handle(acl_msg(Some("irc:chatty"), "two")).await },
            );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the parked message reached the model BEFORE draining — it did not park at all"
        );
        // …and it has not been refused yet either. This is the ordering
        // that makes the claim DRAIN-time rather than arrival-time: a
        // claim taken before the chat lock would refuse here, while the
        // long turn is still running, and every assertion below would
        // still pass.
        let mut early = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if m.kind == MessageKind::Reply {
                early.push(m.text);
            }
        }
        assert!(
            !early.iter().any(|t| t.contains("ceiling for user")),
            "the parked message was refused at ARRIVAL, before the turn it queued behind finished: {early:?}"
        );

        // Release the long turn: the lock drops, the parked message
        // drains, and it claims — into a budget the first turn emptied.
        gate.notify_one();
        let _ = first.await;
        let _ = second.await;

        let mut texts = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if m.kind == MessageKind::Reply {
                texts.push(m.text);
            }
        }
        assert!(
            texts.iter().any(|t| t.contains("pong")),
            "the long turn should still have answered: {texts:?}"
        );
        let refusal = texts
            .iter()
            .find(|t| t.contains("ceiling for user"))
            .unwrap_or_else(|| {
                panic!("the drained message was not refused at the claim: {texts:?}")
            });
        assert!(
            refusal.contains("Try again in"),
            "the refusal must say when to come back: {refusal}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "a turn refused at the claim must not reach the model"
        );
    }

    /// `/queue` and `/steer` answer BEFORE the turn ends, even on a
    /// connector: they ride the pre-lock path, so a command typed while a
    /// turn runs answers at once instead of queuing behind the per-chat
    /// lock that turn holds. The write half (`/steer`) reaches the BUSY
    /// session's agent through its std `pending_steer` Arc, which never
    /// blocks on the turn it must redirect.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn queue_and_steer_answer_while_a_turn_is_running() {
        let (url, gate, parked, _seen) = gated_llm().await;
        let (core, mut rx, _home, _guard) = filter_core(HookBus::new(None), &url);

        // Start a turn that stalls at the provider while holding the chat
        // lock — so a command that had to wait on that lock would never
        // answer until `gate` opens.
        let c1 = Arc::clone(&core);
        let turn =
            veles_core::rt::spawn(
                async move { c1.handle(acl_msg(Some("irc:chatty"), "one")).await },
            );
        for _ in 0..400 {
            if parked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            parked.load(std::sync::atomic::Ordering::SeqCst),
            "the turn never reached the provider"
        );

        // `/queue` answers NOW — before `gate` opens.
        let c2 = Arc::clone(&core);
        let q =
            veles_core::rt::spawn(
                async move { c2.handle(acl_msg(Some("irc:chatty"), "/queue")).await },
            );
        let q_ok = tokio::time::timeout(std::time::Duration::from_secs(3), q)
            .await
            .is_ok();
        assert!(q_ok, "`/queue` did not answer while the turn ran");

        // `/steer` likewise, and it lands on the running agent.
        let c3 = Arc::clone(&core);
        let s = veles_core::rt::spawn(async move {
            c3.handle(acl_msg(Some("irc:chatty"), "/steer use python"))
                .await
        });
        let s_ok = tokio::time::timeout(std::time::Duration::from_secs(3), s)
            .await
            .is_ok();
        assert!(s_ok, "`/steer` did not answer while the turn ran");

        gate.notify_one();
        let _ = turn.await;

        let mut texts = Vec::new();
        while let Ok(out) = rx.try_recv() {
            if out.kind == MessageKind::Reply {
                texts.push(out.text);
            }
        }
        assert!(
            texts.iter().any(|t| t.contains("steer queued")),
            "the steer ack must ride its own reply: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("queue")),
            "the queue read must have answered: {texts:?}"
        );
    }

    /// Two people in one channel run side by side; one person's own
    /// messages still queue behind their own turn and drain after it.
    ///
    /// The turn lock was keyed on the CHAT, so everyone in #flashsupport
    /// waited on one lock: "a" was told "queued #1 — a turn is running;
    /// it runs next" behind the owner's forty-minute turn (2026-10-13),
    /// with the row parked in a's session where the owner's drain never
    /// looks. USER: "the sessions should definitely run concurrently".
    ///
    /// And since their lines now interleave in one stream, a line for a
    /// different session than the room's previous one is addressed to
    /// its owner. USER: "make it address the session owner when it
    /// changes context … because it is a shared stream".
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn two_people_in_one_channel_run_side_by_side() {
        let (url, gate, parked, seen) = gated_llm().await;
        let (core, mut rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\n\
             roles: {\"irc:chatty\": user, \"irc:other\": user}\n",
        );
        let said = |id: &str, nick: &str, text: &str| {
            let mut m = acl_msg(Some(id), text);
            m.sender = nick.into();
            m
        };
        let replies = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>| {
            let mut out = Vec::new();
            while let Ok(m) = rx.try_recv() {
                if m.kind == MessageKind::Reply {
                    out.push(m.text);
                }
            }
            out
        };
        // Same drain, keeping the kind: the parked ack's KIND is the
        // property, not just its text.
        let acks = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>| {
            let mut out = Vec::new();
            while let Ok(m) = rx.try_recv() {
                out.push((m.kind, m.text));
            }
            out
        };

        // chatty's turn parks at the provider, holding chatty's lock.
        let c1 = Arc::clone(&core);
        let msg = said("irc:chatty", "chatty", "one");
        let first = veles_core::rt::spawn(async move { c1.handle(msg).await });
        for _ in 0..400 {
            if parked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            parked.load(std::sync::atomic::Ordering::SeqCst),
            "chatty's turn never reached the provider, so nothing is being proved"
        );

        // other, in the SAME channel, runs now — not after chatty.
        let c2 = Arc::clone(&core);
        let msg = said("irc:other", "other", "two");
        let ran = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            veles_core::rt::spawn(async move { c2.handle(msg).await }),
        )
        .await;
        assert!(ran.is_ok(), "other's turn waited on chatty's");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "other's turn reached the model"
        );
        let early = replies(&mut rx);
        assert_eq!(
            early,
            vec!["pong".to_string()],
            "other is answered at once, unqueued, and as the room's first line \
             it needs no address: {early:?}"
        );

        // The cron and wake paths hold only a session id; they see the
        // same per-session locks.
        let sid = |id: &str| {
            let key = core.session_key_for("irc", "#chan", Some(id), &Default::default());
            core.session_ids
                .lock_ok()
                .get(&key)
                .cloned()
                .expect("session")
        };
        use veles_core::cron::SessionRunner as _;
        let runner = GatewaySessionRunner {
            core: Arc::clone(&core),
        };
        assert!(
            runner.try_lock_session(&sid("irc:other")).is_some(),
            "other's session is free while chatty's turn runs"
        );
        assert!(
            runner.try_lock_session(&sid("irc:chatty")).is_none(),
            "chatty's own session is busy"
        );

        // chatty's second message parks behind chatty's own turn…
        core.handle(said("irc:chatty", "chatty", "three")).await;
        let parked_ack = acks(&mut rx);
        assert!(
            parked_ack
                .iter()
                .any(|(k, t)| *k == MessageKind::Step && t.starts_with("queued #1")),
            "one person's messages still run in order: {parked_ack:?}"
        );
        // …and as a STEP, never a Reply. A Reply is what a platform
        // publishes: atproto posts only a Reply, so this ack went out as
        // a post on a public Bluesky feed ("queued #1 — a turn is
        // running; it runs next" as somebody's answer, live 2026-10-16).
        // A Step is the framework's own narration, which IRC, telegram and
        // the desktop all render and atproto drops.
        assert!(
            !parked_ack.iter().any(|(k, _)| *k == MessageKind::Reply),
            "the parked ack is not a publishable reply: {parked_ack:?}"
        );

        // …and drains when it ends. chatty's answer follows other's line,
        // so it is addressed; the drained one follows chatty's own, so it
        // is not.
        gate.notify_one();
        let _ = first.await;
        for _ in 0..200 {
            if seen.lock().unwrap().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(seen.lock().unwrap().len(), 3, "the parked message drained");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let late = replies(&mut rx);
        assert_eq!(
            late,
            vec!["chatty: pong".to_string(), "pong".to_string()],
            "addressed on the switch, plain after it: {late:?}"
        );

        // And back: other's next line follows chatty's.
        core.handle(said("irc:other", "other", "four")).await;
        let back = replies(&mut rx);
        assert_eq!(back, vec!["other: pong".to_string()], "{back:?}");
    }

    /// A row drained at a turn boundary answers under ITS OWN message,
    /// not under the message that started the turn.
    ///
    /// The drain loop read `msg.msg_id` for every iteration, and `msg` is
    /// the turn-STARTING message — the one whose lock the row parked
    /// behind. On a threaded platform that sent the drained answer to the
    /// wrong place, twice over, and both were observed live on Bluesky
    /// (2026-10-16):
    ///
    /// - the in-turn drain posted the answer under a post two messages
    ///   and twenty minutes up-thread from the question it answered, so
    ///   it was invisible to anyone reading the exchange;
    /// - `parked_inbound` fabricated a `queued:<nanos>-<seq>` id that is
    ///   not an at-uri, so atproto's `deliver_reply` could not resolve
    ///   refs for it and refused to post rather than dump it top-level on
    ///   a public feed. The answer was silently dropped, with only the
    ///   sender's `session_mail.reply` row as a trace.
    ///
    /// This pins the first: two messages with distinct ids, the second
    /// parked behind the first's turn, and the drained reply must carry
    /// the SECOND id.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_drained_row_answers_under_its_own_message() {
        let (url, gate, parked, seen) = gated_llm().await;
        let (core, mut rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\n\
             roles: {\"irc:chatty\": user}\n",
        );
        let said = |id: &str, nick: &str, text: &str| {
            let mut m = acl_msg(Some(id), text);
            m.sender = nick.into();
            m
        };
        let replies = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>| {
            let mut out = Vec::new();
            while let Ok(m) = rx.try_recv() {
                if m.kind == MessageKind::Reply {
                    out.push((m.text, m.reply_to));
                }
            }
            out
        };
        // The parked ack is a Step now, so a Reply-only drain would see
        // nothing where the ack is expected.
        let texts = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>| {
            let mut out = Vec::new();
            while let Ok(m) = rx.try_recv() {
                if matches!(m.kind, MessageKind::Reply | MessageKind::Step) && !m.text.is_empty() {
                    out.push(m.text);
                }
            }
            out
        };

        // The first message starts the turn and parks at the provider.
        let c1 = Arc::clone(&core);
        let first_msg = said("irc:chatty", "chatty", "one");
        let first_id = first_msg.msg_id.clone();
        let first = veles_core::rt::spawn(async move { c1.handle(first_msg).await });
        for _ in 0..200 {
            if parked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            parked.load(std::sync::atomic::Ordering::SeqCst),
            "the first turn is at the provider, holding the lock"
        );
        texts(&mut rx);

        // The second message arrives on the same session and parks.
        let second_msg = said("irc:chatty", "chatty", "two");
        let second_id = second_msg.msg_id.clone().expect("a platform id");
        assert_ne!(
            second_id,
            first_id.clone().expect("a platform id"),
            "the two messages have distinct platform ids, or the test proves nothing"
        );
        core.handle(second_msg).await;
        let ack = texts(&mut rx);
        assert!(
            ack.iter().any(|t| t.starts_with("queued #1")),
            "the second line parked: {ack:?}"
        );

        // The row is parked with the second message's id recorded as its
        // target — the datum the drain now replays.
        let key = core.session_key_for(
            "irc",
            "#chan",
            Some("irc:chatty"),
            &veles_core::acl::SenderContext::default(),
        );
        let sid = core
            .session_ids
            .lock_ok()
            .get(&key)
            .cloned()
            .expect("session");
        let rows = core.store.lock_ok().queue_items(&sid).unwrap();
        assert_eq!(rows.len(), 1, "one row parked: {rows:?}");
        assert_eq!(
            rows[0].reply_to.as_deref(),
            Some(second_id.as_str()),
            "the row records WHERE its answer belongs"
        );

        // Let the first turn finish; its boundary drains the row.
        gate.notify_one();
        let _ = first.await;
        for _ in 0..200 {
            if seen.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "both turns reached the provider"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let out = replies(&mut rx);
        assert_eq!(out.len(), 2, "two answers: {out:?}");
        assert_eq!(
            out[0].1, first_id,
            "the first answer threads under the message that started it"
        );
        assert_eq!(
            out[1].1,
            Some(second_id),
            "the DRAINED answer threads under its own message, not the \
             turn-starting one — this is the bug: it used to carry the \
             first id, so a threaded platform posted it under an \
             unrelated earlier post"
        );
    }

    /// A plain line that parked behind a SLASH COMMAND runs when the
    /// command ends. Only a turn's boundary drained the queue, and a
    /// command runs no turn: the line waited for the sender's next turn
    /// and ran after it, out of order (found 2026-10-13). The test stands
    /// in for whatever held the lock first; `/status` is the command that
    /// waits on it, and its end kicks the parked row back through the
    /// inbound pipeline, which this rig keeps.
    #[tokio::test]
    async fn a_line_parked_behind_a_command_runs_when_the_command_ends() {
        let (url, seen) = capturing_llm().await;
        let (core, mut irc, _tg, mut inbound, _home, _guard) = join_core(&url);
        core.handle(alice("hi")).await;
        drain_texts(&mut irc);
        let key = core.session_key_for("irc", "#chan", Some("irc:alice"), &alice("x").sender_ctx);

        let lock = core.turn_lock(&key);
        let held = lock.lock_owned().await;
        let c = Arc::clone(&core);
        let cmd = veles_core::rt::spawn(async move { c.handle(alice("/status")).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        core.handle(alice("are you there")).await;
        let acks = drain_texts(&mut irc);
        assert!(
            acks.iter().any(|t| t.starts_with("queued #1")),
            "the line parks behind the held lock: {acks:?}"
        );
        assert!(
            inbound.try_recv().is_err(),
            "nothing re-enters while the lock is held"
        );

        drop(held);
        cmd.await.unwrap();
        let status = drain_texts(&mut irc);
        assert!(
            !status.is_empty(),
            "the command answered once it had the lock"
        );
        let m = inbound
            .try_recv()
            .expect("the command's end kicks the parked line back into the pipeline");
        assert_eq!(m.text, "are you there");
        let before = seen.lock().unwrap().len();
        core.handle(m).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            before + 1,
            "the parked line ran"
        );
        assert!(seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("are you there"));
        assert!(
            core.store
                .lock_ok()
                .queue_items(&core.session_ids.lock_ok()[&key])
                .unwrap()
                .is_empty(),
            "nothing left parked"
        );
    }

    /// A sender without the `chat` cap is refused at the park, not
    /// parked: every turn of theirs is refused, so a parked row would only
    /// ever be refused later, after holding a seat in the queue.
    #[tokio::test]
    async fn a_sender_without_chat_is_refused_not_parked() {
        let (url, _seen) = capturing_llm().await;
        let (core, mut rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\n\
             roles: {\"irc:mute\": user}\n\
             deny_caps: {\"irc:mute\": [\"chat\"]}\n",
        );
        let m = acl_msg(Some("irc:mute"), "let me in");
        let key = core.session_key_for("irc", "#chan", Some("irc:mute"), &m.sender_ctx);
        let lock = core.turn_lock(&key);
        let _held = lock.lock_owned().await;
        core.handle(m).await;
        let mut texts = Vec::new();
        while let Ok(out) = rx.try_recv() {
            texts.push(out.text);
        }
        assert!(
            texts
                .iter()
                .any(|t| t.contains("agent turns are not available to")),
            "refused by name: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.starts_with("queued")),
            "never parked: {texts:?}"
        );
        assert!(
            core.session_ids.lock_ok().get(&key).is_none(),
            "no session is made for a sender who cannot chat"
        );
    }

    // ── the owner, joined to someone else's session (`/join`) ──

    /// A gateway with IRC and Telegram, whose inbound side the test
    /// keeps: a queued line re-enters through it, as in the live loop.
    #[allow(clippy::type_complexity)]
    fn join_core(
        base_url: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tokio::sync::mpsc::UnboundedReceiver<InboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        let (tmp, guard) = veles_core::testenv::scratch_home();
        let (core, irc_rx) = budget_core_with_at(
            tmp.path(),
            base_url,
            "owner: [\"telegram:1\"]\nroles: {\"irc:alice\": user}\n",
            veles_core::config::InjectionsSection::default(),
        );
        let (tg_tx, tg_rx) = tokio::sync::mpsc::unbounded_channel();
        core.bus
            .outbound
            .lock()
            .unwrap()
            .insert("telegram".into(), HashMap::from([(String::new(), tg_tx)]));
        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        // The harness drops its inbound side; this one is kept.
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::clone(&core.bus.outbound),
                last_chat: Arc::clone(&core.bus.last_chat),
                state_path: core.bus.state_path.clone(),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store: Arc::clone(&core.store),
            cfg: Arc::clone(&core.cfg),
            cfg_from_file: false,
            hooks: HookBus::new(None),
        });
        (core, irc_rx, tg_rx, in_rx, tmp, guard)
    }

    /// alice, in #chan on IRC.
    fn alice(text: &str) -> InboundMessage {
        let mut m = acl_msg(Some("irc:alice"), text);
        m.sender = "alice".into();
        m.sender_ctx.mask = Some("alice!~a@host".into());
        m
    }

    /// The owner, in their Telegram DM with the bot.
    fn owner_tg(text: &str) -> InboundMessage {
        let mut m = acl_msg(Some("telegram:1"), text);
        m.platform = "telegram".into();
        m.chat_id = "1".into();
        m.sender = "wrath".into();
        m.sender_ctx.handle = Some("wrath".into());
        m.sender_ctx.direct = true;
        m
    }

    fn drain_texts(rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if matches!(m.kind, MessageKind::Reply | MessageKind::Step) && !m.text.is_empty() {
                out.push(m.text);
            }
        }
        out
    }

    /// The whole loop the USER asked for (2026-10-12/13): the owner joins
    /// alice's session from Telegram; their line shows in alice's chat as
    /// theirs and runs there as alice's session's next turn, which the
    /// model is told is the owner's; the answer reaches alice AND comes
    /// back to the owner, and so does alice's next line, which the model
    /// is told is alice's. `/leave` gives the owner their own chat back.
    #[tokio::test]
    async fn the_owner_joins_a_users_session_and_speaks_in_it() {
        let (url, seen) = capturing_llm().await;
        let (core, mut irc, mut tg, mut inbound, _home, _guard) = join_core(&url);

        core.handle(alice("hi")).await;
        assert_eq!(drain_texts(&mut irc), vec!["pong".to_string()]);
        let key = core.session_key_for("irc", "#chan", Some("irc:alice"), &alice("x").sender_ctx);
        let sid = core
            .session_ids
            .lock_ok()
            .get(&key)
            .cloned()
            .expect("alice's session");

        // The list, then the join.
        core.handle(owner_tg("/join")).await;
        let list = drain_texts(&mut tg).join("\n");
        assert!(
            list.contains(&sid) && list.contains("[alice in #chan]"),
            "{list}"
        );
        core.handle(owner_tg("/join alice")).await;
        let joined = drain_texts(&mut tg).join("\n");
        assert!(joined.starts_with("joined [alice in #chan]"), "{joined}");
        assert_eq!(
            veles_core::join::active(&sid)
                .map(|j| j.owner_name.clone())
                .as_deref(),
            Some("wrath")
        );

        // The owner's line: alice's chat sees it as the owner's at once,
        // and it waits in alice's queue as the session's next turn.
        let before = seen.lock().unwrap().len();
        core.handle(owner_tg("alice, try it with -v")).await;
        assert_eq!(
            drain_texts(&mut irc),
            vec!["wrath (owner): alice, try it with -v".to_string()]
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            before,
            "no turn of the owner's own"
        );
        core.kick_parked_queue(&sid);
        let m = inbound.try_recv().expect("the queued line re-enters");
        core.handle(m).await;
        let req = seen.lock().unwrap().last().cloned().unwrap();
        assert!(
            req.contains("wrath, the owner of this agent, has joined"),
            "{req}"
        );
        assert!(req.contains("alice did not write it"), "{req}");
        assert!(req.contains("alice, try it with -v"));
        assert!(
            req.contains("You are NOT talking to the owner"),
            "alice's session keeps alice's prompt"
        );
        assert_eq!(
            drain_texts(&mut irc),
            vec!["pong".to_string()],
            "alice reads the answer"
        );
        assert_eq!(
            drain_texts(&mut tg),
            vec!["[alice in #chan] pong".to_string()],
            "the owner reads it too, and not their own line back"
        );

        // alice's next line: mirrored to the owner, named for the model.
        core.handle(alice("ok thanks")).await;
        let req = seen.lock().unwrap().last().cloned().unwrap();
        assert!(
            req.contains("arrived via the irc surface, from alice."),
            "{req}"
        );
        assert_eq!(drain_texts(&mut irc), vec!["pong".to_string()]);
        assert_eq!(
            drain_texts(&mut tg),
            vec![
                "[alice in #chan] <alice> ok thanks".to_string(),
                "[alice in #chan] pong".to_string()
            ]
        );

        // /leave: the owner's chat is theirs again, and alice's is hers.
        core.handle(owner_tg("/leave")).await;
        assert!(drain_texts(&mut tg)[0].starts_with("left [alice in #chan]"));
        assert!(veles_core::join::active(&sid).is_none());
        let n = seen.lock().unwrap().len();
        core.handle(owner_tg("and now just me")).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            n + 1,
            "a turn of the owner's own"
        );
        let req = seen.lock().unwrap().last().cloned().unwrap();
        assert!(!req.contains("has joined this conversation"), "{req}");
        assert_eq!(drain_texts(&mut tg), vec!["pong".to_string()]);
        assert!(drain_texts(&mut irc).is_empty(), "nothing reaches alice");
        core.handle(alice("bye")).await;
        assert!(
            drain_texts(&mut tg).is_empty(),
            "nothing of alice's reaches the owner"
        );
        // The history still holds the joined turns' rows; the NEWEST
        // origin is plain again.
        let req = seen.lock().unwrap().last().cloned().unwrap();
        // The plain row now carries a `; answering on …; caller …`
        // suffix, so "nobody else is here" = the surface phrase is NOT
        // followed by ", from <who>." (a join names a second person).
        let o = "[origin] this message arrived via the irc surface";
        let last = req.rfind(o).expect("an origin row");
        assert!(
            !req[last + o.len()..].starts_with(", from"),
            "nobody else is here any more: {}",
            &req[last..(last + 160).min(req.len())]
        );
    }

    /// A live ACL change also closes an ordinary IRC/Telegram join, not
    /// only the Desktop endpoint. No participant message is needed to
    /// refresh the cached role before the owner's next joined line.
    #[tokio::test]
    async fn a_live_acl_ban_refuses_shared_join_input() {
        let (mut core, mut irc, mut tg, _in, _home, _guard) = join_core("http://127.0.0.1:0/v1");
        let sid = core
            .test_sender("irc", "#chan", "irc:alice", alice("x").sender_ctx)
            .unwrap();
        core.handle(owner_tg("/join alice")).await;
        assert!(drain_texts(&mut tg).join("\n").starts_with("joined"));
        assert!(core
            .store
            .lock_ok()
            .meta_get("join:gateway:v1:telegram:1")
            .is_some());
        Arc::get_mut(&mut core).unwrap().cfg_from_file = true;
        std::fs::write(
            veles_core::config::config_dir().join("config.toml"),
            "[model]\nprovider = \"fixture\"\ndefault = \"fixture-model\"\n\n\
             [[custom_providers]]\nname = \"fixture\"\n\
             base_url = \"http://127.0.0.1:0/v1\"\nmodels = [\"fixture-model\"]\n\n\
             [acl]\nowner = [\"telegram:1\"]\n\n\
             [acl.roles]\n\"irc:alice\" = \"banned\"\n",
        )
        .unwrap();
        core.handle(owner_tg("must not become an ordinary owner turn"))
            .await;
        assert!(core.store.lock_ok().queue_items(&sid).unwrap().is_empty());
        assert!(drain_texts(&mut irc).is_empty());
        assert!(drain_texts(&mut tg).is_empty());
        assert!(veles_core::join::active(&sid).is_some());
        core.handle(owner_tg("/leave")).await;
        assert!(drain_texts(&mut tg)[0].starts_with("left"));
        assert!(veles_core::join::active(&sid).is_none());
    }

    /// Joining is the owner's alone: anyone else is refused by name, and
    /// is not shown the command in their help.
    #[tokio::test]
    async fn join_is_the_owners_alone() {
        let (url, seen) = capturing_llm().await;
        let (core, mut irc, _tg, _in, _home, _guard) = join_core(&url);
        core.handle(alice("/join wrath")).await;
        let out = drain_texts(&mut irc).join("\n");
        assert!(out.contains("[acl] `join` is the owner's"), "{out}");
        core.handle(alice("/leave")).await;
        assert!(drain_texts(&mut irc)
            .join("\n")
            .contains("[acl] `leave` is the owner's"));
        assert!(seen.lock().unwrap().is_empty(), "no turn");
        core.handle(alice("/help")).await;
        let help = drain_texts(&mut irc).join("\n");
        assert!(!help.contains("/join"), "{help}");
    }

    /// The owner's line while alice's turn runs: it queues behind that
    /// turn (the owner is told), and alice's own drain at the turn's end
    /// runs it as the owner's line.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn the_owners_line_waits_behind_a_running_turn_and_drains_as_theirs() {
        let (url, gate, parked, seen) = gated_llm().await;
        let (core, mut irc, mut tg, _in, _home, _guard) = join_core(&url);
        let c1 = Arc::clone(&core);
        let first = veles_core::rt::spawn(async move { c1.handle(alice("long one")).await });
        for _ in 0..400 {
            if parked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            parked.load(std::sync::atomic::Ordering::SeqCst),
            "alice's turn is running"
        );
        core.handle(owner_tg("/join alice")).await;
        let _ = drain_texts(&mut tg);
        core.handle(owner_tg("meanwhile, check the log")).await;
        assert!(
            drain_texts(&mut tg)
                .iter()
                .any(|t| t.starts_with("(queued: [alice in #chan] has a turn running")),
            "the owner is told it waits"
        );
        assert!(
            drain_texts(&mut irc).contains(&"wrath (owner): meanwhile, check the log".to_string())
        );
        gate.notify_one();
        let _ = first.await;
        let log = seen.lock().unwrap().clone();
        assert_eq!(log.len(), 2, "alice's turn, then the owner's line");
        assert!(log[1].contains("wrath, the owner of this agent, has joined"));
        assert!(log[1].contains("meanwhile, check the log"));
        let back = drain_texts(&mut tg);
        assert!(
            back.contains(&"[alice in #chan] pong".to_string()),
            "{back:?}"
        );
        core.handle(owner_tg("/leave")).await;
    }

    /// The addressing rule on its own: only lines every connector shows
    /// count, a DM is never addressed, and the model's own address is not
    /// doubled.
    #[test]
    fn a_voice_addresses_the_owner_only_when_the_room_changes_hands() {
        let speakers: Arc<RoomSpeakers> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let voice = |speaker: &str, name: &str| Voice {
            tx: tx.clone(),
            room: Some(Arc::new(VoiceRoom {
                speakers: Arc::clone(&speakers),
                room: "irc:#chan".into(),
                speaker: speaker.into(),
                name: name.into(),
            })),
            tees: Vec::new(),
        };
        let (a, b) = (voice("irc:#chan:a", "a"), voice("irc:#chan", "owner"));
        let mut line = |v: &Voice, kind: MessageKind, text: &str| {
            assert!(v.send(OutboundMessage::plain("#chan", text, kind)));
            rx.try_recv().unwrap().text
        };

        assert_eq!(
            line(&a, MessageKind::Reply, "hi"),
            "hi",
            "the room's first line"
        );
        assert_eq!(line(&a, MessageKind::Step, "more"), "more", "same speaker");
        // Progress is shown or dropped per adapter: never addressed, and
        // it does not take the room.
        assert_eq!(line(&b, MessageKind::Progress, "⚙ terminal"), "⚙ terminal");
        assert_eq!(line(&b, MessageKind::Progress, ""), "");
        assert_eq!(line(&a, MessageKind::Step, "still a"), "still a");
        // The switch.
        assert_eq!(line(&b, MessageKind::Step, "working"), "owner: working");
        assert_eq!(line(&b, MessageKind::Reply, "done"), "done");
        // The model addressed them itself: no stutter, and the room is
        // still taken.
        assert_eq!(
            line(&a, MessageKind::Reply, "A, here you go"),
            "A, here you go"
        );
        assert_eq!(line(&b, MessageKind::Reply, "owner: ok"), "owner: ok");
        assert_eq!(line(&a, MessageKind::Reply, "again"), "a: again");

        // A DM has no room: nothing to address.
        let dm = Voice {
            tx: tx.clone(),
            room: None,
            tees: Vec::new(),
        };
        assert_eq!(line(&dm, MessageKind::Reply, "hi"), "hi");

        // The name: the nick on the wire, not IRC's "nick (account)".
        let mut ctx = veles_core::acl::SenderContext::default();
        assert_eq!(address_name("a (acct)", &ctx).as_deref(), Some("a"));
        ctx.mask = Some("a!~a@86b64c8e".into());
        assert_eq!(
            address_name("", &ctx).as_deref(),
            Some("a"),
            "a parked turn"
        );
        assert_eq!(address_name("", &Default::default()), None);
        assert!(
            !addressed_to("another thing", "a"),
            "a prefix of a word is not an address"
        );
    }

    /// A quiet connector shows no note block — and the note is still in
    /// the transcript, which is the whole distinction.
    ///
    /// Notes rode every reply unconditionally, with one global on/off,
    /// so "quiet in the channel, loud on my desktop" was inexpressible:
    /// silencing guard notes for a room of strangers also silenced them
    /// where the owner reads them. This suppresses one platform's copy
    /// of the narration, never the mechanism.
    #[test]
    fn a_quiet_connector_trims_the_note_block_without_silencing_the_mechanism() {
        use veles_core::notes::NoteKind;
        let plain = veles_core::config::MechanismsSection::default();
        let quiet = veles_core::config::MechanismsSection::quiet_on("irc");
        for kind in NoteKind::all() {
            let k = kind.as_str();
            let bookkeeping = veles_core::config::CONNECTOR_DEFAULT_SILENCED.contains(&k);
            // An unconfigured CONNECTOR silences the bookkeeping kinds
            // and keeps the ones that say what the caller actually got.
            // Was "nothing is silenced by default" until 2026-09-10, when
            // a `🧠 36k of context injected` note in a public channel
            // settled the question of what that default should be.
            assert_eq!(
                plain.silences_on("irc", k),
                bookkeeping,
                "the irc default for {k}"
            );
            assert_eq!(
                plain.silences_on("telegram", k),
                bookkeeping,
                "telegram gets the same default, not irc's complaint: {k}"
            );
            // A surface that is not a connector keeps everything: there
            // the transcript IS the product.
            assert!(
                !plain.silences_on("desktop", k),
                "the desktop must still show {k}"
            );
            assert!(quiet.silences_on("irc", k), "quiet silences {k}");
            // …and only on THAT platform. Telegram answers to its own
            // default rather than to irc's profile, so a kind irc quiets
            // beyond the default is still telegram's to show.
            if !bookkeeping {
                assert!(
                    !quiet.silences_on("telegram", k),
                    "quiet on irc must not reach telegram: {k}"
                );
            }
            assert!(
                !quiet.silences(k),
                "the GLOBAL list is untouched, so the transcript keeps the note: {k}"
            );
        }

        // The fine control still works underneath the profile, and is
        // additive to it.
        let mut fine = veles_core::config::MechanismsSection::default();
        fine.platforms.insert(
            "irc".into(),
            veles_core::config::PlatformNotes {
                verbosity: None,
                silenced: vec!["guard".into()],
            },
        );
        assert!(fine.silences_on("irc", "guard"));
        assert!(
            !fine.silences_on("irc", "compaction"),
            "one entry silences one mechanism, not the lot"
        );
        assert!(!fine.silences_on("desktop", "guard"));
    }

    /// The connector default has an off switch, and the fine list adds to
    /// it rather than replacing it.
    #[test]
    fn the_connector_default_is_one_word_to_undo() {
        use veles_core::config::{MechanismsSection, PlatformNotes, CONNECTOR_DEFAULT_SILENCED};
        use veles_core::notes::NoteKind;
        let with = |v: Option<&str>, silenced: &[&str]| {
            let mut m = MechanismsSection::default();
            m.platforms.insert(
                "irc".into(),
                PlatformNotes {
                    verbosity: v.map(String::from),
                    silenced: silenced.iter().map(|s| (*s).to_string()).collect(),
                },
            );
            m
        };

        // "normal" is spelled exactly like the fallback and means the
        // opposite: somebody looked at it and chose every kind.
        let normal = with(Some("normal"), &[]);
        for k in CONNECTOR_DEFAULT_SILENCED {
            assert!(
                !normal.silences_on("irc", k),
                "an explicit normal restores {k}"
            );
        }

        // Unset keeps the default, and one operator silencing compaction
        // on irc did not thereby ask for the guard narration back.
        let fine = with(None, &["compaction"]);
        assert!(fine.silences_on("irc", "compaction"));
        for k in CONNECTOR_DEFAULT_SILENCED {
            assert!(
                fine.silences_on("irc", k),
                "the default survives an entry: {k}"
            );
        }
        assert!(
            !fine.silences_on("irc", "failover"),
            "and a kept kind stays kept"
        );

        // Loud is the debugging posture: nothing silenced anywhere.
        let loud = with(Some("loud"), &[]);
        for k in NoteKind::all() {
            assert!(
                !loud.silences_on("irc", k.as_str()),
                "loud shows {}",
                k.as_str()
            );
        }

        // A connector nobody configured is still a connector, however it
        // is spelled in a message.
        assert!(MechanismsSection::default().silences_on("IRC", "guard"));
        assert!(MechanismsSection::default().silences_on(" irc ", "memory"));
        assert!(!MechanismsSection::default().silences_on("tui", "guard"));

        // The SET is pinned rather than derived: every other assertion
        // here reads the constant, so a kind moved into it would drag its
        // own expectations along and stay green. The kept half is how a
        // caller learns something about their answer changed. P22.3 is
        // the silenced half's proof on a real wire, using an approval note.
        assert_eq!(
            CONNECTOR_DEFAULT_SILENCED,
            &[
                "guard",
                "memory",
                "injection",
                "cache",
                "plugin",
                "approval"
            ],
            "USER 2026-09-10: \"memory, injection, plugin, guard, cache — silence on irc. \
             failover, approval, compaction, schedule — keep on.\" USER 2026-10-13, after \
             five ⚖ lines in #flashsupport: \"the approvals themselves don't get suppressed \
             but I never intended them to be printed to the channel\""
        );
        assert!(
            !normal.silences_on("irc", "approval"),
            "an operator who wants the ⚖ lines back writes one word"
        );
        for kept in [
            "failover",
            "aux_route",
            "compaction",
            "schedule",
            "plugin_rewrite",
        ] {
            assert!(
                !CONNECTOR_DEFAULT_SILENCED.contains(&kept),
                "{kept} says what the caller GOT changed — it must survive on a connector"
            );
            assert!(
                !MechanismsSection::default().silences_on("irc", kept),
                "and it does survive: {kept}"
            );
        }
    }

    /// A rust plugin whose only surface is a gateway filter.
    struct FilterPlugin(veles_core::rustplugins::FilterDecision, &'static str);
    impl veles_core::rustplugins::VelesPlugin for FilterPlugin {
        fn name(&self) -> &str {
            "gateway-filter-under-test"
        }
        fn version(&self) -> &str {
            "test"
        }
        fn gateway_filter(&self) -> Option<veles_core::rustplugins::GatewayFilterFn> {
            let decision = self.0.clone();
            let trigger = self.1;
            Some(Arc::new(move |_p: &str, _c: &str, text: &str| {
                (text.trim() == trigger).then(|| decision.clone())
            }))
        }
    }

    /// A rust plugin's SKIP must stop the message dead: no turn, no
    /// outbound, not even an error.
    // Holds the env lock across `.await`, which clippy flags. It is
    // correct here and the alternative is worse: the lock is what stops
    // two tests repointing VELES_HOME under each other, and the whole
    // point of `testenv` is that a fixture must hold its guard for the
    // WHOLE test rather than releasing it when the fixture returns.
    // Nothing can deadlock on it — each `#[tokio::test]` gets its own
    // single-threaded runtime on its own thread, so a waiting test
    // blocks its own thread and no executor is starved.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_rust_filter_skip_stops_the_message_at_the_gateway() {
        let (url, seen) = capturing_llm().await;
        // The fixture's guard holds the env lock for the WHOLE body
        // (see acl_core_with); the plugin registry is process-global,
        // so the registration rides inside that lock too.
        let (core, mut rx, _home, _guard) = filter_core(HookBus::new(None), &url);
        veles_core::rustplugins::register_plugin(Arc::new(FilterPlugin(
            veles_core::rustplugins::FilterDecision::Skip,
            "!drop",
        )));

        core.handle(acl_msg(Some("irc:someone"), "!drop")).await;
        assert!(
            rx.try_recv().is_err(),
            "a skipped message must produce no outbound at all"
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "and must never reach the model"
        );
    }

    /// A rust plugin's REWRITE must change what the MODEL is asked, not
    /// merely what the handler returned. Asserted on the request body,
    /// because that is the only place the substitution is visible.
    // Holds the env lock across `.await`, which clippy flags. It is
    // correct here and the alternative is worse: the lock is what stops
    // two tests repointing VELES_HOME under each other, and the whole
    // point of `testenv` is that a fixture must hold its guard for the
    // WHOLE test rather than releasing it when the fixture returns.
    // Nothing can deadlock on it — each `#[tokio::test]` gets its own
    // single-threaded runtime on its own thread, so a waiting test
    // blocks its own thread and no executor is starved.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_rust_filter_rewrite_changes_what_the_model_is_asked() {
        let (url, seen) = capturing_llm().await;
        // As in the skip test: the fixture's guard is the env lock, and
        // the global registration happens under it.
        let (core, _rx, _home, _guard) = filter_core(HookBus::new(None), &url);
        veles_core::rustplugins::register_plugin(Arc::new(FilterPlugin(
            veles_core::rustplugins::FilterDecision::Rewrite("REWRITTEN BY THE PLUGIN".to_string()),
            "!swap",
        )));

        core.handle(acl_msg(Some("irc:someone"), "!swap")).await;
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "the turn must have run: {requests:?}");
        assert!(
            requests[0].contains("REWRITTEN BY THE PLUGIN"),
            "the gateway must send the REWRITTEN text: {}",
            requests[0]
        );
        assert!(
            !requests[0].contains("!swap"),
            "and not the original: {}",
            requests[0]
        );
    }

    /// The TCL half, through the same path: the example plugin's
    /// `pre_gateway_dispatch` handler rewrites `!ping`, and the model
    /// must be asked the canned prompt instead.
    ///
    /// Checked through the gateway rather than by calling
    /// `alias_filter::handle`, which is what the existing sample test
    /// does — that proves a proc returns a verdict, not that anything
    /// consumes it.
    // Holds the env lock across `.await`, which clippy flags. It is
    // correct here and the alternative is worse: the lock is what stops
    // two tests repointing VELES_HOME under each other, and the whole
    // point of `testenv` is that a fixture must hold its guard for the
    // WHOLE test rather than releasing it when the fixture returns.
    // Nothing can deadlock on it — each `#[tokio::test]` gets its own
    // single-threaded runtime on its own thread, so a waiting test
    // blocks its own thread and no executor is starved.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_tcl_pre_gateway_dispatch_rewrite_reaches_the_model() {
        let (home, _guard) = veles_core::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");
        let examples = format!("{}/../../plugins/examples", env!("CARGO_MANIFEST_DIR"));
        std::env::set_var("VELES_PLUGIN_DIRS", format!("{examples}/tcl-alias-filter"));
        let tcl = veles_core::tclrt::spawn(home.path().to_path_buf()).expect("tcl spawn");
        // spawn returns before plugins load; the hook registry would be
        // empty if we looked now.
        veles_core::rt::spawn_blocking({
            let tcl = tcl.clone();
            move || tcl.wait_ready()
        })
        .await
        .expect("join")
        .expect("tcl ready");
        std::env::remove_var("VELES_PLUGIN_DIRS");

        let (url, seen) = capturing_llm().await;
        let (core, _rx) = filter_core_at(home.path(), HookBus::new(Some(tcl)), &url);

        core.handle(acl_msg(Some("irc:someone"), "!ping")).await;
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "the turn must have run: {requests:?}");
        assert!(
            requests[0].contains("Reply with exactly one word: pong"),
            "the Tcl filter's rewrite must reach the model: {}",
            requests[0]
        );
    }

    /// IRC rolling-context parity (the `irc_context` injection):
    /// `InboundMessage.context` lands in the transcript as a TAGGED
    /// row whatever the knob says — visibility is never negotiable —
    /// and the model reads it only while
    /// `agent.injections.irc_context` is on.
    fn filter_core_with_irc_knob(
        hooks: Arc<HookBus>,
        base_url: &str,
        irc_context_on: bool,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        let (tmp, guard) = veles_core::testenv::scratch_home();
        let (core, rx) = filter_core_with_irc_knob_at(tmp.path(), hooks, base_url, irc_context_on);
        (core, rx, tmp, guard)
    }

    fn filter_core_with_irc_knob_at(
        dir: &std::path::Path,
        hooks: Arc<HookBus>,
        base_url: &str,
        irc_context_on: bool,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
    ) {
        filter_core_with_platform_knob_at(dir, hooks, base_url, "irc", irc_context_on, false)
    }

    /// The rolling-context harness, parametric in platform. Registers the
    /// platform's outbound route and sets that platform's context knob —
    /// `irc_on` for irc, `atproto_on` for atproto — leaving the other at
    /// its default, so a test can prove the knobs gate independently.
    fn filter_core_with_platform_knob_at(
        dir: &std::path::Path,
        hooks: Arc<HookBus>,
        base_url: &str,
        platform: &str,
        irc_on: bool,
        atproto_on: bool,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
    ) {
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert(
            platform.to_string(),
            HashMap::from([(String::new(), out_tx)]),
        );
        let cfg = veles_core::config::VelesConfig {
            model: veles_core::config::ModelSection {
                provider: Some("fixture".into()),
                default: Some("fixture-model".into()),
                ..Default::default()
            },
            custom_providers: vec![veles_core::config::ProviderEntry {
                name: "fixture".into(),
                base_url: base_url.to_string(),
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            agent: veles_core::config::AgentSection {
                injections: veles_core::config::InjectionsSection {
                    irc_context: irc_on,
                    atproto_context: atproto_on,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks,
        });
        (core, out_rx)
    }

    #[tokio::test]
    async fn irc_context_lands_as_a_tagged_row_and_the_model_reads_it() {
        let (url, seen) = capturing_llm().await;
        let (core, _rx, _home, _guard) = filter_core_with_irc_knob(HookBus::new(None), &url, true);
        let mut m = acl_msg(Some("irc:someone"), "hello there");
        m.context = Some("10:00 <wrath> earlier line".into());
        core.handle(m).await;
        // The model's request carries the block (knob on).
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "the turn must have run: {requests:?}");
        assert!(
            requests[0].contains("earlier line"),
            "the model reads the context when the knob is on: {}",
            requests[0]
        );
        // The transcript row is a TAGGED injection — never the user's
        // own words, always labelled irc_context.
        let session = core.sessions.lock().unwrap();
        let agent = session.get("irc:#chan").expect("session exists");
        let history = agent
            .store
            .lock()
            .unwrap()
            .load_history(&agent.session_id)
            .expect("history");
        let row = history
            .iter()
            .find(|m| m.injection.as_deref() == Some("irc_context"))
            .expect("the context row is tagged irc_context");
        assert_eq!(row.content, "10:00 <wrath> earlier line");
    }

    #[tokio::test]
    async fn a_muted_irc_context_is_still_in_the_transcript() {
        let (url, seen) = capturing_llm().await;
        let (core, _rx, _home, _guard) = filter_core_with_irc_knob(HookBus::new(None), &url, false);
        let mut m = acl_msg(Some("irc:someone"), "hello there");
        m.context = Some("10:00 <wrath> earlier line".into());
        core.handle(m).await;
        let requests = seen.lock().unwrap().clone();
        assert!(
            !requests[0].contains("earlier line"),
            "with the knob off the model does not spend context on it: {}",
            requests[0]
        );
        let session = core.sessions.lock().unwrap();
        let agent = session.get("irc:#chan").expect("session exists");
        let history = agent
            .store
            .lock()
            .unwrap()
            .load_history(&agent.session_id)
            .expect("history");
        assert!(
            history
                .iter()
                .any(|m| m.injection.as_deref() == Some("irc_context")
                    && m.content.contains("earlier line")),
            "the row stays visible in the transcript even when muted"
        );
    }

    #[tokio::test]
    async fn atproto_context_is_tagged_and_gated_by_its_own_knob() {
        // An atproto turn's context block is tagged `atproto_context`
        // (not `irc_context`) and the model reads it only while the
        // atproto_context knob is on — the two surfaces gate independently.
        let (url, seen) = capturing_llm().await;
        let (tmp, _guard) = veles_core::testenv::scratch_home();
        let (core, _rx) = filter_core_with_platform_knob_at(
            tmp.path(),
            HookBus::new(None),
            &url,
            "atproto",
            false, // irc_context OFF — must not matter here
            true,  // atproto_context ON
        );
        let mut m = acl_msg(Some("atproto:did:plc:someone"), "hello there");
        m.platform = "atproto".into();
        m.chat_id = "at://did:plc:x/app.bsky.feed.post/root".into();
        m.context = Some("you: earlier reply\nalice: said this".into());
        core.handle(m).await;
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "the turn ran: {requests:?}");
        assert!(
            requests[0].contains("said this"),
            "the atproto context is read when its own knob is on (irc's is off): {}",
            requests[0]
        );
        let session = core.sessions.lock().unwrap();
        let agent = session
            .get("atproto:at://did:plc:x/app.bsky.feed.post/root")
            .expect("atproto session exists");
        let history = agent
            .store
            .lock()
            .unwrap()
            .load_history(&agent.session_id)
            .expect("history");
        let row = history
            .iter()
            .find(|m| m.injection.as_deref() == Some("atproto_context"))
            .expect("the context row is tagged atproto_context, not irc_context");
        assert!(row.content.contains("said this"));
    }

    // ── session lifecycle commands + per-step flushing ───────────────

    /// Extract the session id out of a "(new session <sid>)" reply.
    fn sid_of(reply: &str) -> String {
        reply
            .strip_prefix("(new session ")
            .and_then(|t| t.strip_suffix(')'))
            .unwrap_or_else(|| panic!("reply must name the session: {reply}"))
            .to_string()
    }

    /// The services-less case, which is now the normal one: nobody has
    /// an account, so `sender_id` is None for everyone, and two people
    /// in one channel are told apart by their WIRE MASK.
    ///
    /// Before this they were not told apart at all — `session_key_for`
    /// keyed on `Principal.id`, so every id-less sender in a channel
    /// collapsed onto ONE session with one history, one frozen prompt
    /// and one sandbox. Reverting the key to `id` turns this red.
    #[tokio::test]
    async fn mask_only_senders_get_their_own_sessions() {
        let (core, _rx, _home, _guard) = acl_core();
        let ctx_for = |mask: &str| veles_core::acl::SenderContext {
            mask: Some(mask.into()),
            network: Some("localtest".into()),
            channel: Some("#chan".into()),
            platform: Some("irc".into()),
            chat_id: Some("#chan".into()),
            direct: false,
            handle: None,
            post: None,
        };
        let a = ctx_for("alice!~a@box.example");
        let b = ctx_for("bob!~b@box.example");
        let ka = core.session_key_for("irc", "#chan", None, &a);
        let kb = core.session_key_for("irc", "#chan", None, &b);
        assert_ne!(ka, kb, "two masks in one channel are two sessions");
        assert!(
            ka.contains("alice!~a@box.example"),
            "the key names the mask that identified them: {ka}"
        );
        // The same person on another network is a different subject
        // (a nick!user@host is not globally unique).
        let mut elsewhere = a.clone();
        elsewhere.network = Some("othernet".into());
        assert_ne!(ka, core.session_key_for("irc", "#chan", None, &elsewhere));
        // And building the agents really does produce two.
        let _ = core
            .agent_for_sender("irc", "#chan", None, &a)
            .expect("alice");
        let _ = core
            .agent_for_sender("irc", "#chan", None, &b)
            .expect("bob");
        let sessions = core.sessions.lock().unwrap();
        assert!(sessions.contains_key(&ka) && sessions.contains_key(&kb));
        assert_eq!(sessions.len(), 2, "{:?}", sessions.keys());
    }

    /// A background completion in an IDLE chat-backed session starts a
    /// real turn in it: the model reads the `[BACKGROUND SHELL
    /// COMPLETE]` row, and its answer reaches the chat — with no route
    /// footer, because the message was not prompted. This is the
    /// USER's contract: completion arrives in the session and STARTS
    /// an agent turn when the session was idle.
    #[tokio::test]
    async fn an_idle_chat_session_wakes_when_a_background_task_completes() {
        let (url, gate, parked, seen) = gated_llm().await;
        let (core, mut rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\nroles: {\"irc:other\": user}\n",
        );
        // Create the session the way a message would: agent_for_sender
        // hydrates the meta reverse-map the wake resolves through.
        let agent = core
            .agent_for_sender("irc", "#chan", Some("irc:owner"), &Default::default())
            .expect("owner session");
        let sid = agent.session_id.clone();
        // The completion arrives: delegate event (what the busy-path
        // drain would read) + the bus event (what the wake loop
        // reads).
        veles_core::delegate::push_background_event(
            &sid,
            veles_core::delegate::BackgroundEvent {
                child_id: 42,
                kind: veles_core::tasks::TaskKind::Shell,
                goal: "make the thing".into(),
                result: serde_json::json!({"summary": "made"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        veles_core::session_tap::publish(
            &sid,
            "shell-complete",
            serde_json::json!({
                "session_id": sid,
                "id": 42,
                "command": "make the thing",
                "exit_code": 0,
                "tail": "made",
            }),
        );

        // The wake itself, called the way the loop calls it.
        let c = core.clone();
        let wake = veles_core::rt::spawn(async move {
            c.gateway_wake_one(
                &c.bus.clone(),
                &veles_core::session_wake::WakeEvent {
                    kind: "shell-complete".into(),
                    session_id: sid.clone(),
                },
            )
            .await
        });

        // The provider has the request; release the gate so the turn
        // can finish.
        for _ in 0..400 {
            if parked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            parked.load(std::sync::atomic::Ordering::SeqCst),
            "the wake never reached the provider"
        );
        gate.notify_waiters();
        wake.await.expect("wake join").expect("wake ok");
        // The MODEL read the completion row: the request it made
        // carries the user-role [BACKGROUND SHELL COMPLETE] text.
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "exactly one provider call: {requests:?}");
        assert!(
            requests[0].contains("[BACKGROUND SHELL COMPLETE]"),
            "the model's request must carry the completion, not an empty prompt: {}",
            requests[0]
        );
        assert!(
            requests[0].contains("make the thing"),
            "the drained row names the command: {}",
            requests[0]
        );

        // The reply reaches the chat, footer-free.
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("reply within 5s")
            .expect("outbound open");
        assert_eq!(reply.kind, MessageKind::Reply);
        assert_eq!(
            reply.provider, "",
            "a wake turn's reply carries no runtime footer — it was not prompted"
        );
        assert_eq!(reply.model, "");
        assert!(
            reply.text.contains("pong"),
            "the model's answer: {}",
            reply.text
        );
    }

    /// The engine clears its live-run row just before the surface drops
    /// its chat lock. A post-turn retry must survive that short handoff,
    /// not mistake the old lock for a reason to strand the completion.
    #[tokio::test]
    async fn a_post_turn_completion_waits_for_the_old_chat_lock_then_wakes() {
        let (url, gate, _parked, seen) = gated_llm().await;
        let (core, _rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\nroles: {\"irc:other\": user}\n",
        );
        let agent = core
            .agent_for_sender("irc", "#chan", Some("irc:owner"), &Default::default())
            .expect("owner session");
        let sid = agent.session_id.clone();
        let speaker = core.session_lock_key(&sid).expect("chat lock key");
        let lock = core.turn_lock(&speaker);
        let held = lock.lock().await;
        veles_core::delegate::push_background_event(
            &sid,
            veles_core::delegate::BackgroundEvent {
                child_id: 81,
                kind: veles_core::tasks::TaskKind::Shell,
                goal: "finished in the last response".into(),
                result: serde_json::json!({"summary": "arrived after the last drain"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        let c = core.clone();
        let wake_sid = sid.clone();
        let wake = veles_core::rt::spawn(async move {
            c.gateway_wake_one(
                &c.bus.clone(),
                &veles_core::session_wake::WakeEvent {
                    kind: "background-wake-pending".into(),
                    session_id: wake_sid,
                },
            )
            .await
        });
        // The wake is admitted but cannot bill while the old surface
        // lock is held. Release it only after its task has had a poll.
        tokio::task::yield_now().await;
        assert!(seen.lock().unwrap().is_empty(), "no call before unlock");
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while seen.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the retry was abandoned with the old chat lock");
        gate.notify_waiters();
        wake.await.expect("wake join").expect("wake ok");
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "exactly one provider call");
        assert!(requests[0].contains("[BACKGROUND SHELL COMPLETE]"));
        assert!(requests[0].contains("finished in the last response"));
    }

    /// The BUSY case stands down: a turn in flight (any surface — the
    /// store's session_runs row is cross-process) means the running
    /// turn's own drain delivers the completion at its next step. The
    /// wake must not race it, and the completion rows must still be
    /// there for that drain.
    #[tokio::test]
    async fn a_busy_chat_session_does_not_start_a_second_turn() {
        let (url, _gate, _parked, seen) = gated_llm().await;
        let (core, _rx, _home, _guard) = budget_core(
            &url,
            "owner: [\"irc:owner\"]\nroles: {\"irc:other\": user}\n",
        );
        let agent = core
            .agent_for_sender("irc", "#chan", Some("irc:owner"), &Default::default())
            .expect("owner session");
        let sid = agent.session_id.clone();
        // A live session_runs row: some surface's turn in flight.
        core.store
            .lock()
            .unwrap()
            .run_start(&sid, std::process::id(), "tester", "irc")
            .unwrap();
        veles_core::delegate::push_background_event(
            &sid,
            veles_core::delegate::BackgroundEvent {
                child_id: 43,
                kind: veles_core::tasks::TaskKind::Shell,
                goal: "noise".into(),
                result: serde_json::json!({"summary": "noise"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );

        core.gateway_wake_one(
            &core.bus.clone(),
            &veles_core::session_wake::WakeEvent {
                kind: "shell-complete".into(),
                session_id: sid.clone(),
            },
        )
        .await
        .expect("wake stands down cleanly");

        assert!(
            seen.lock().unwrap().is_empty(),
            "a busy session must not start a second turn"
        );
        assert!(
            veles_core::delegate::has_background_events(&sid),
            "the completion must still be queued for the running turn's drain"
        );
    }

    /// A muted session (agent.injections.background off) never wakes:
    /// the completion would drain into a suppressed note, so a wake
    /// turn would bill the provider for nothing the model may read.
    #[tokio::test]
    async fn a_muted_session_does_not_bill_a_wake_turn() {
        let (url, _gate, _parked, seen) = gated_llm().await;
        let (core, _rx, _home, _guard) = budget_core_with(
            &url,
            "owner: [\"irc:owner\"]\nroles: {\"irc:other\": user}\n",
            veles_core::config::InjectionsSection {
                background: false,
                ..Default::default()
            },
        );
        let agent = core
            .agent_for_sender("irc", "#chan", Some("irc:owner"), &Default::default())
            .expect("owner session");
        let sid = agent.session_id.clone();
        veles_core::delegate::push_background_event(
            &sid,
            veles_core::delegate::BackgroundEvent {
                child_id: 44,
                kind: veles_core::tasks::TaskKind::Shell,
                goal: "muted".into(),
                result: serde_json::json!({"summary": "muted"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        core.gateway_wake_one(
            &core.bus.clone(),
            &veles_core::session_wake::WakeEvent {
                kind: "shell-complete".into(),
                session_id: sid.clone(),
            },
        )
        .await
        .expect("muted wake stands down");
        assert!(
            seen.lock().unwrap().is_empty(),
            "a muted session must not bill a provider call"
        );
        assert!(
            veles_core::delegate::has_background_events(&sid),
            "the completion is queued for the next real turn"
        );
    }
    #[tokio::test]
    async fn a_live_session_picks_up_a_role_change() {
        let (core, _rx, _home, _guard) = acl_core();
        let agent = core
            .agent_for_sender("irc", "#chan", Some("irc:friend"), &Default::default())
            .expect("friend session");
        assert_eq!(
            agent.principal().map(|p| p.role),
            Some(veles_core::acl::Role::Trusted),
            "acl_core pins friend as trusted"
        );
        // Demote them in the LIVE store (a grant is the runtime half of
        // the ACL, and `.agent grant` / the desktop UI write exactly
        // this).
        veles_core::acl::set_runtime_pairing_store(core.store.clone());
        veles_core::pairing::grant_scoped(&core.store, "irc:friend", "banned", None, None, None)
            .expect("grant");
        let again = core
            .agent_for_sender("irc", "#chan", Some("irc:friend"), &Default::default())
            .expect("same session");
        assert_eq!(
            again.principal().map(|p| p.role),
            Some(veles_core::acl::Role::Banned),
            "the running session must see the demotion"
        );
        assert!(
            again.principal().is_some_and(|p| p.caps.is_empty()),
            "and lose its capabilities with it"
        );
        veles_core::pairing::revoke(&core.store, "irc:friend");
    }

    /// `/cancel` stops the CALLER's turn, not "the chat's".
    ///
    /// `session_ids` used to hold one entry per chat, overwritten by
    /// whichever session was created last, so in a shared channel the
    /// owner's `/cancel` could stop a guest's turn (or vice versa) and
    /// either way the caller was told it worked. Two senders in one
    /// channel now have two entries, and each `/cancel` resolves its
    /// own.
    #[tokio::test]
    async fn cancel_resolves_the_callers_own_session() {
        let (core, mut rx, _home, _guard) = acl_core();
        // Two senders in the SAME chat, each with their own session.
        core.handle(acl_msg(Some("irc:owner"), "/new")).await;
        let owner_sid = sid_of(&rx.try_recv().expect("owner /new answers").text);
        core.handle(acl_msg(Some("irc:friend"), "/new")).await;
        let friend_sid = sid_of(&rx.try_recv().expect("friend /new answers").text);
        assert_ne!(owner_sid, friend_sid, "two senders, two sessions");

        let map = core.session_ids.lock().unwrap().clone();
        assert_eq!(
            map.get("irc:#chan"),
            Some(&owner_sid),
            "the owner's session key IS the plain chat key: {map:?}"
        );
        assert_eq!(
            map.get("irc:#chan:irc:friend"),
            Some(&friend_sid),
            "a non-owner is keyed per sender: {map:?}"
        );

        // Nothing is running, so both are told so — but the point is
        // WHICH session each one looked at, which the map above pins.
        // A sender with no session at all resolves nothing rather than
        // reaching for someone else's.
        core.handle(acl_msg(Some("irc:owner"), "/cancel")).await;
        let out = rx.try_recv().expect("cancel answers");
        assert!(out.text.contains("nothing running"), "{}", out.text);
    }

    /// `/detach` rides the same pre-lock fast lane as `/cancel`: it
    /// answers from the session map without touching the agent lock a
    /// running turn holds, refuses a guest at the cmd-session tier,
    /// and arms the flag only when the caller's own session is inside
    /// a foreground terminal command.
    #[tokio::test]
    async fn detach_answers_from_the_fast_lane_and_resolves_the_callers_session() {
        let (core, mut rx, _home, _guard) = acl_core();
        core.handle(acl_msg(Some("irc:owner"), "/new")).await;
        let owner_sid = sid_of(&rx.try_recv().expect("owner /new answers").text);

        // A guest is denied at the tier BEFORE any session lookup —
        // same guarantee /cancel's fast path was fixed for.
        core.handle(acl_msg(Some("irc:stranger"), "/detach")).await;
        let out = rx.try_recv().expect("guest gets a /detach denial");
        assert!(
            out.text.contains("requires a higher access level"),
            "guest /detach must be denied by the cmd-session tier: {}",
            out.text
        );
        assert!(
            !veles_core::agent::detach_requested(&owner_sid),
            "a denied /detach must not arm anything"
        );

        // Owner, no turn running: honest refusal, nothing armed.
        core.handle(acl_msg(Some("irc:owner"), "/detach")).await;
        let out = rx.try_recv().expect("owner /detach answers");
        assert!(
            out.text.contains("nothing running"),
            "with no turn the answer says so: {}",
            out.text
        );

        // Owner, live turn, but not inside a foreground command: the
        // middle refusal — the safety property against arming between
        // tool rounds.
        let tok = veles_core::providers::CancelToken::new();
        veles_core::agent::register_turn_for_test(&owner_sid, tok);
        core.handle(acl_msg(Some("irc:owner"), "/detach")).await;
        let out = rx.try_recv().expect("owner /detach answers");
        assert!(
            out.text.contains("nothing to detach"),
            "a turn outside a fg command must be refused: {}",
            out.text
        );
        assert!(
            !veles_core::agent::detach_requested(&owner_sid),
            "and must not arm the flag"
        );

        // Inside a foreground command: armed, and the reply says the
        // command keeps running.
        veles_core::agent::mark_fg_command(&owner_sid, true);
        core.handle(acl_msg(Some("irc:owner"), "/detach")).await;
        let out = rx.try_recv().expect("owner /detach answers");
        assert!(
            out.text.contains("keeps running"),
            "the armed answer promises the move, not a kill: {}",
            out.text
        );
        assert!(
            veles_core::agent::detach_requested(&owner_sid),
            "the tool's next poll sees the flag"
        );
        veles_core::agent::mark_fg_command(&owner_sid, false);
        veles_core::agent::clear_turn_for_test(&owner_sid);
    }

    /// `/new` and `/reset` rebind the chat to a FRESH session — the fix
    /// for the 2026-09-07 hang where `/new` fell through to the model as
    /// a literal prompt. No LLM call happens; the reply is immediate.
    #[tokio::test]
    async fn new_and_reset_start_a_fresh_session() {
        let (core, mut rx, _home, _guard) = acl_core();
        core.handle(acl_msg(Some("irc:owner"), "/new")).await;
        let out = rx.try_recv().expect("owner /new answers");
        let sid = sid_of(&out.text);
        assert_eq!(
            core.session_ids.lock().unwrap().get("irc:#chan"),
            Some(&sid),
            "the chat must point at the fresh session"
        );
        let agent = core
            .sessions
            .lock()
            .unwrap()
            .get("irc:#chan")
            .unwrap()
            .clone();
        assert_eq!(agent.session_id, sid, "the owner's agent must be rebound");

        // /reset is the same command
        core.handle(acl_msg(Some("irc:owner"), "/reset")).await;
        let out = rx.try_recv().expect("owner /reset answers");
        assert_eq!(out.kind, MessageKind::Reply);
        let sid2 = sid_of(&out.text);
        assert_ne!(sid2, sid, "each /new must create a distinct session");
    }

    /// A chat whose session left the node (migrated, or taken over while
    /// this node was away) carries on in a new session here and says so.
    /// The fence refused every message in the old one, with a note about
    /// "archived" rows the person in the chat could do nothing about.
    #[tokio::test]
    async fn a_chat_whose_session_moved_away_continues_in_a_new_one() {
        let (core, mut rx, _home, _guard) = acl_core();
        core.handle(acl_msg(Some("irc:owner"), "/new")).await;
        let _ = rx.try_recv();
        let old = live_sid(&core);
        core.store
            .lock()
            .unwrap()
            .conn()
            .execute(
                "UPDATE sessions SET node = 'migrated_to:node-x@1' WHERE id = ?1",
                [&old],
            )
            .unwrap();
        core.handle(acl_msg(Some("irc:owner"), "hello")).await;
        let mut said = Vec::new();
        while let Ok(m) = rx.try_recv() {
            said.push(m.text);
        }
        assert!(
            said.iter()
                .any(|t| t.contains(&old) && t.contains("lives on node node-x")),
            "{said:?}"
        );
        let new = live_sid(&core);
        assert_ne!(new, old, "the chat is bound to a new session");
        assert!(core.store.lock().unwrap().session_exists(&new).unwrap());
    }

    /// Session lifecycle commands are session-shaping: a guest must not
    /// be able to wipe or hop the owner's session.
    #[tokio::test]
    async fn a_guest_cannot_new_or_resume_the_chat_session() {
        let (core, mut rx, _home, _guard) = acl_core();
        core.handle(acl_msg(Some("irc:stranger"), "/new")).await;
        let out = rx.try_recv().expect("guest gets a /new denial");
        assert!(
            out.text.contains("requires a higher access level"),
            "guest /new must be denied by the cmd-session tier, not the chat gate: {}",
            out.text
        );
        core.handle(acl_msg(Some("irc:stranger"), "/resume abc"))
            .await;
        let out = rx.try_recv().expect("guest gets a /resume denial");
        assert!(
            out.text.contains("requires a higher access level"),
            "guest /resume must be denied by the cmd-session tier, not the chat gate: {}",
            out.text
        );
    }

    /// `/resume` hops back to a known session by id or prefix; an
    /// unknown prefix explains itself instead of going to the model.
    #[tokio::test]
    async fn resume_returns_to_a_known_session_by_prefix() {
        let (core, mut rx, _home, _guard) = acl_core();
        core.handle(acl_msg(Some("irc:owner"), "/new")).await;
        let first = sid_of(&rx.try_recv().unwrap().text);
        core.handle(acl_msg(Some("irc:owner"), "/new")).await;
        let _ = rx.try_recv().unwrap();

        core.handle(acl_msg(Some("irc:owner"), &format!("/resume {first}")))
            .await;
        let out = rx.try_recv().expect("/resume answers");
        assert!(
            out.text.starts_with("(resumed "),
            "resume must land on the session: {}",
            out.text
        );

        core.handle(acl_msg(Some("irc:owner"), "/resume zzznope"))
            .await;
        let out = rx.try_recv().expect("no match answers");
        assert!(
            out.text.contains("no session matching"),
            "a miss must be reported, not sent to the model: {}",
            out.text
        );
    }

    /// The core of "flush each step": a two-round turn (round one
    /// announces and calls a tool, round two answers) delivers the
    /// intermediate message as a `Step` and the last round as the Reply —
    /// not one dump at the end.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_multi_round_turn_flushes_each_step_instead_of_one_dump() {
        let round1 = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"searching\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"terminal\",\"arguments\":\"{\\\"command\\\":\\\"echo ok\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let round2 = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"the answer\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let url = veles_core::providers::tests::sse_server_seq(vec![
            round1.to_string(),
            round2.to_string(),
        ])
        .await;
        let (core, mut rx, _home, _guard) = filter_core(HookBus::new(None), &url);

        // A PRIVATE chat: the progress window is process-global and
        // keyed by chat key, and under parallel test threads another
        // `#chan` test's tail flush can consume this turn's ⚙ window
        // between note and flush — the marker then lands in nobody's
        // assertion. This test owns its whole chat.
        let chat = private_chat();
        let mut m = acl_msg(Some("irc:someone"), "question");
        m.chat_id = chat.clone();
        core.handle(m).await;

        let mut steps = Vec::new();
        let mut tool_markers = Vec::new();
        let mut reply = String::new();
        while let Ok(out) = rx.try_recv() {
            match out.kind {
                MessageKind::Step => steps.push(out.text),
                MessageKind::Progress if out.text.starts_with('⚙') => tool_markers.push(out.text),
                MessageKind::Reply => reply = out.text,
                _ => {}
            }
        }
        assert!(
            tool_markers.iter().any(|t| t.contains("terminal")),
            "the tool round must be announced: {tool_markers:?}"
        );
        assert!(
            steps.iter().any(|s| s.contains("searching")),
            "the intermediate message must flush as a Step: {steps:?}"
        );
        assert!(
            reply.contains("the answer"),
            "the final Reply carries the last round's answer (notes may ride along): {reply}"
        );
        assert!(
            !steps.iter().any(|s| s.contains("the answer")),
            "the final answer must not be duplicated as a Step: {steps:?}"
        );
    }

    /// An engine note is its own message, delivered where it happened —
    /// not batched to the head of the reply after a multi-tool turn has
    /// finished narrating. A compact (the note this turns on) fires the
    /// note at turn start, so the message that says "history shrunk" must
    /// land BEFORE the reply, as a Step, without crowding the answer.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_engine_note_delivers_as_its_own_message_not_batched_to_the_reply() {
        let turn = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        // The compaction summarizer is a NON-streaming call, so it needs
        // a JSON (not SSE) response of its own.
        let summarizer =
            "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"the head, summarized\"}}]}";
        let url = veles_core::providers::tests::sse_server_seq(vec![
            turn.to_string(),
            summarizer.to_string(),
            turn.to_string(),
        ])
        .await;

        // A scratch config whose compaction bar is tiny so a small seeded
        // history crosses it — the note does not depend on a live
        // provider reporting tokens, so the estimate path is the test.
        let (home, _guard) = veles_core::testenv::scratch_home();
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert("irc".to_string(), HashMap::from([(String::new(), out_tx)]));
        let cfg = veles_core::config::VelesConfig {
            model: veles_core::config::ModelSection {
                provider: Some("fixture".into()),
                default: Some("fixture-model".into()),
                ..Default::default()
            },
            custom_providers: vec![veles_core::config::ProviderEntry {
                name: "fixture".into(),
                base_url: url.clone(),
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            agent: veles_core::config::AgentSection {
                compaction: veles_core::config::CompactionSection {
                    threshold_chars: 120,
                    keep_recent: 4,
                    min_head: 3,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let dir = home.path().to_path_buf();
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store: store.clone(),
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks: HookBus::new(None),
        });

        // First turn creates the session and seeds a little history.
        core.handle(acl_msg(Some("irc:someone"), "hello")).await;
        // Seed enough user/assistant rows to make a legal head (a user
        // boundary above min_head) and cross the 120-char hard bar.
        let sid = store
            .lock()
            .unwrap()
            .list_sessions(1)
            .unwrap()
            .into_iter()
            .next()
            .map(|(id, _)| id)
            .expect("first turn made a session");
        for i in 0..4 {
            store
                .lock()
                .unwrap()
                .append_message(
                    &sid,
                    &ChatMessage::user(&format!("seed user {i} {}", "x".repeat(30))),
                )
                .unwrap();
            store
                .lock()
                .unwrap()
                .append_message(
                    &sid,
                    &ChatMessage::assistant(&format!("seed assistant {i}"), None),
                )
                .unwrap();
        }

        // Second turn: the compaction note fires at turn start, before
        // the reply.
        core.handle(acl_msg(Some("irc:someone"), "second question"))
            .await;

        let mut steps = Vec::new();
        let mut reply = String::new();
        while let Ok(out) = out_rx.try_recv() {
            match out.kind {
                MessageKind::Step => steps.push(out.text),
                MessageKind::Reply => reply = out.text,
                _ => {}
            }
        }
        assert!(
            steps.iter().any(|s| s.contains("compacted history")),
            "the compaction note must arrive as its own Step: {steps:?}"
        );
        assert!(
            !reply.contains("compacted history"),
            "the reply must not carry the note block: {reply}"
        );
        assert!(
            reply.contains("ok"),
            "and the answer still arrives: {reply}"
        );
    }

    /// The #coven flood, pinned: 12 tool calls to one ⚙ summary, not 12
    /// PRIVMSGs — plus the whole-turn narration budget, which is what
    /// actually reserves room for the answer a caller is waiting for.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn tool_progress_is_one_summary_per_window_not_one_line_per_call() {
        // Round 1 chains six terminal calls: exactly one full window,
        // narrated as ONE line from the first to the last. Round 2 chains
        // 24 more — past the narration budget that window caps instead,
        // and the answer still lands. Each call needs its own INDEX:
        // the stream parser accumulates by index, and one index per
        // call is how a real provider streams a parallel tool burst.
        let call = |n: usize| {
            let chunk = serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": n,
                            "id": format!("call_{n}"),
                            "function": {
                                "name": "terminal",
                                "arguments": format!("{{\"command\":\"echo {n}\"}}"),
                            }
                        }]
                    }
                }]
            })
            .to_string();
            format!("data: {chunk}\n\n")
        };
        let mut round1 = String::new();
        for i in 0..6 {
            round1.push_str(&call(i));
        }
        round1.push_str(
            "data: {\"choices\":[{\"delta\":{\"content\":\"more work\"}}]}\n\ndata: [DONE]\n\n",
        );
        let mut round2 = String::new();
        for i in 0..24 {
            round2.push_str(&call(i));
        }
        round2.push_str("data: [DONE]\n\n");
        // The turn only ends when a round yields no tool call, so the
        // answer is its own third provider call.
        let round3 = sse_round("the answer");
        let url = veles_core::providers::tests::sse_server_seq(vec![round1, round2, round3]).await;
        let (core, mut rx, _home, _guard) = filter_core(HookBus::new(None), &url);

        core.handle(acl_msg(Some("irc:someone"), "question")).await;

        let mut markers = Vec::new();
        let mut reply = None;
        while let Ok(out) = rx.try_recv() {
            match out.kind {
                MessageKind::Progress if out.text.starts_with('⚙') => markers.push(out.text),
                MessageKind::Reply => reply = Some(out.text),
                _ => {}
            }
        }
        // 30 calls → a line per six (budget 3) and one cap line at the
        // tail: four ⚙ messages for the whole turn, where the old code
        // sent thirty.
        assert_eq!(
            markers.len(),
            4,
            "30 calls must be 4 lines, not 30: {markers:?}"
        );
        assert!(
            markers
                .iter()
                .filter(|m| m.contains("6 calls") && m.contains("terminal"))
                .count()
                == 3,
            "each full window narrates once, naming the tool: {markers:?}"
        );
        assert!(
            markers
                .iter()
                .filter(|m| m.contains("not narrated"))
                .count()
                == 1,
            "past the budget the window caps once instead of narrating: {markers:?}"
        );
        assert!(
            reply.unwrap_or_default().contains("the answer"),
            "the answer still lands after the narration wraps"
        );
    }

    /// The wrapper resolution, pinned as a pure function: the name a
    /// summary shows for a DEFERRED call is the inner target, never
    /// `tool_call` — which is what made thirty live lines identical.
    #[test]
    fn progress_windows_resolve_deferred_tool_names() {
        let name = ProgressWindow::resolve_progress_name(
            "tool_call",
            r#"{"name":"terminal","arguments":{"command":"echo"}}"#,
        );
        assert_eq!(name, "terminal", "the wrapper must not be the label");

        let literal = ProgressWindow::resolve_progress_name("terminal", r#"{"command":"echo"}"#);
        assert_eq!(literal, "terminal", "a literal call keeps its own name");

        // Garbage (a deferred call whose inner name is missing) stays
        // the wrapper rather than inventing a label.
        let broken = ProgressWindow::resolve_progress_name("tool_call", "not json");
        assert_eq!(broken, "tool_call");
    }

    /// The budget is a brake, not a slower flood: past the cap the
    /// window says so ONCE and afterwards stays silent whatever notes.
    #[test]
    fn progress_budget_says_once_then_stays_silent() {
        let mut w = ProgressWindow::default();
        let mut lines = Vec::new();
        for _ in 0..6 {
            for _ in 0..6 {
                if let Some(l) = w.note("terminal", "{}") {
                    lines.push(l);
                }
            }
        }
        // 36 calls / 6 = 6 lines. Budget 3 narrates the first three
        // windows, the fourth line is the cap text, the rest silent.
        let budget_lines = lines.iter().filter(|l| l.contains("not narrated")).count();
        assert_eq!(
            budget_lines, 1,
            "the cap text appears exactly once: {lines:?}"
        );
        assert_eq!(
            lines.len(),
            4,
            "6 windows later: 3 summaries + 1 cap line, then silence: {lines:?}"
        );
    }

    /// Two cores in one process must not share a chat's progress budget.
    ///
    /// They did. `progress_windows()` was a process-global `static` keyed
    /// by chat, while every sibling piece of per-chat turn state —
    /// `turn_locks`, `sessions`, `turn_permits` — lives on the core. In
    /// production that is indistinguishable (one core per process);
    /// under `cargo test` it is not, because tests run in parallel
    /// threads in ONE process and every gateway test here sends to
    /// `#chan`. `tail_progress` REMOVES the entry, so one test ending
    /// its turn handed another a fresh budget mid-flight and an extra ⚙
    /// line — the CI failure of 2026-09-17, five lines where the budget
    /// allows four, green on every developer box and red on a
    /// differently-scheduled runner.
    #[tokio::test]
    async fn one_cores_turn_end_cannot_refresh_anothers_progress_budget() {
        // No provider is contacted: this exercises the window bookkeeping
        // the two cores keep, not a turn. One scratch home for both —
        // a second scratch_home would deadlock on the env lock, which
        // is not reentrant (testenv.rs).
        let (home, _guard) = veles_core::testenv::scratch_home();
        let (a, _ra) = filter_core_at(home.path(), HookBus::new(None), "http://127.0.0.1:0");
        let (b, _rb) = filter_core_at(home.path(), HookBus::new(None), "http://127.0.0.1:0");
        let key = "irc:#chan";

        // A spends its whole budget: three summaries and the cap line.
        let mut spent = Vec::new();
        for _ in 0..24 {
            if let Some(l) = note_progress(&a.progress, key, "terminal", "{}") {
                spent.push(l);
            }
        }
        assert_eq!(
            spent.len(),
            4,
            "3 summaries + the cap line is the whole budget: {spent:?}"
        );

        // B's turn ends in a chat with the SAME id. Nothing of A's moves.
        tail_progress(&b.progress, key);

        // A keeps calling tools. Its budget is spent, so it stays silent.
        let mut after = Vec::new();
        for _ in 0..12 {
            if let Some(l) = note_progress(&a.progress, key, "terminal", "{}") {
                after.push(l);
            }
        }
        assert!(
            after.is_empty(),
            "another core's turn end must not hand this one a fresh budget: {after:?}"
        );
    }

    // ── the marker channel, at the seam that knows the principal ──────────
    //
    // `media::deliverable` and its own tests pin the RULE; these pin that the
    // gateway actually asks it, in both directions, for every line a turn
    // sends. A rule with no caller is a comment.

    /// One SSE round carrying `content`, escaped by serde rather than by hand.
    fn sse_round(content: &str) -> String {
        let chunk = serde_json::json!({"choices":[{"delta":{"content": content}}]}).to_string();
        format!("data: {chunk}\n\ndata: [DONE]\n\n")
    }

    /// A scratch VELES_HOME with a media root, a file inside it, and a secret
    /// outside it whose contents are distinctive enough to prove it was never
    /// opened. Returns (home, allowed_path, secret_path, secret_contents).
    fn media_fixture_at(
        home: &std::path::Path,
        tag: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, String) {
        // The CALLER owns the home (its scratch_home guard is what
        // publishes VELES_HOME for the whole test body); this only
        // fills it. The old media_fixture set the var itself with no
        // lock at all and never restored it.
        std::fs::create_dir_all(home.join("media")).unwrap();
        veles_core::acl::invalidate_live_config();

        let allowed = home.join("media").join("chart.png");
        std::fs::write(&allowed, b"png-bytes").unwrap();
        let note = home.join("media").join("note.txt");
        std::fs::write(&note, b"MEDIA-ROOT-CONTENT").unwrap();
        let secret = home.join("secrets.env");
        let contents = format!("TOKEN=unique-secret-value-{tag}");
        std::fs::write(&secret, contents.as_bytes()).unwrap();
        (allowed, secret, contents)
    }

    /// These tests route below-owner principals (`irc:alice`,
    /// `irc:someone`) into the sandbox, and under `testenv` the sandbox
    /// must stay off the developer's real podman. It did not: every gate
    /// run left three `veles-user-env-dm_irc_*` containers behind, keyed
    /// to scratch homes nobody would use again (244 by 2026-10-09).
    #[tokio::test]
    async fn test_user_envs_stay_off_the_real_podman() {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let err = veles_core::user_envs::ensure("dm_irc_offline-pin")
            .await
            .expect_err("a test made a real sandbox box");
        assert!(err.to_string().contains("offline under tests"), "{err}");
    }

    const MEDIA_ACL: &str = "owner: [\"irc:owner\"]\nroles: {\"irc:alice\": user}";

    /// Outbound, reply: a below-owner principal's reply keeps the marker that
    /// names the media root and loses the one that names a host file. The note
    /// that replaces it carries no path — the channel is public, and "where
    /// the secret lives" is exactly what the channel should not learn.
    ///
    /// Mutation: delete the `gate(...)` call on the reply path and the host
    /// path reaches the wire; `media::boundary_tests` all still pass, because
    /// the rule was never the thing under test here.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_below_owner_reply_cannot_carry_a_host_file_out() {
        let (home, _guard) = veles_core::testenv::scratch_home();
        let (allowed, secret, contents) = media_fixture_at(home.path(), "reply");
        let round = sse_round(&format!(
            "here are the files\nMEDIA:{}\nMEDIA:{}",
            secret.display(),
            allowed.display()
        ));
        let url = veles_core::providers::tests::sse_server_seq(vec![round]).await;
        let (core, mut rx) = budget_core_at(home.path(), &url, MEDIA_ACL);

        core.handle(acl_msg(Some("irc:alice"), "question")).await;

        let out = next_reply(&mut rx).expect("a reply");
        assert!(
            out.text.contains(&format!("MEDIA:{}", allowed.display())),
            "the media root is deliverable, so the file is not lost: {}",
            out.text
        );
        assert!(
            !out.text.contains(&secret.display().to_string()),
            "no host path on the wire: {}",
            out.text
        );
        assert!(!out.text.contains(&contents), "{}", out.text);
        assert!(
            out.text.contains(veles_core::media::WITHHELD_NOTE),
            "and the channel is told something was withheld: {}",
            out.text
        );
    }

    /// The same rule, other direction of the same principal: an OWNER's reply
    /// keeps every marker, because the owner's own bot handing the owner their
    /// own file is not an escalation. If this ever fails, the gate has been
    /// applied to the wrong side of the ACL rather than to the wrong path.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_owner_reply_keeps_every_marker() {
        let (home, _guard) = veles_core::testenv::scratch_home();
        let (allowed, secret, _contents) = media_fixture_at(home.path(), "owner");
        let round = sse_round(&format!(
            "here are the files\nMEDIA:{}\nMEDIA:{}",
            secret.display(),
            allowed.display()
        ));
        let url = veles_core::providers::tests::sse_server_seq(vec![round]).await;
        let (core, mut rx) = budget_core_at(home.path(), &url, MEDIA_ACL);

        core.handle(acl_msg(Some("irc:owner"), "question")).await;

        let out = next_reply(&mut rx).expect("a reply");
        assert!(
            out.text.contains(&format!("MEDIA:{}", secret.display())),
            "an owner's marker survives: {}",
            out.text
        );
        assert!(
            !out.text.contains(veles_core::media::WITHHELD_NOTE),
            "{}",
            out.text
        );
    }

    /// Outbound, intermediate rounds: a marker can ride a STEP just as easily
    /// as a reply, and a step is model text too. Gating only the reply would
    /// leave the multi-round path open, which is the path a tool-using turn
    /// actually takes.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_intermediate_step_is_gated_like_the_reply() {
        let (home, _guard) = veles_core::testenv::scratch_home();
        let (_allowed, secret, contents) = media_fixture_at(home.path(), "step");
        let round1 = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"delta":{"content": format!("fetching\nMEDIA:{}", secret.display())}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"terminal","arguments":"{\"command\":\"echo ok\"}"}}]}}]})
        );
        let url =
            veles_core::providers::tests::sse_server_seq(vec![round1, sse_round("the answer")])
                .await;
        let (core, mut rx) = budget_core_at(home.path(), &url, MEDIA_ACL);

        core.handle(acl_msg(Some("irc:alice"), "question")).await;

        let mut sent = Vec::new();
        while let Ok(out) = rx.try_recv() {
            sent.push((out.kind, out.text));
        }
        assert!(
            sent.iter().any(|(k, t)| matches!(k, MessageKind::Step)
                && t.contains(veles_core::media::WITHHELD_NOTE)),
            "the step is gated where the reply is: {sent:?}"
        );
        assert!(
            !sent
                .iter()
                .any(|(_, t)| t.contains(&secret.display().to_string())),
            "no line of any kind carries the host path: {sent:?}"
        );
        assert!(!sent.iter().any(|(_, t)| t.contains(&contents)), "{sent:?}");
    }

    /// Inbound: a marker in a sender's own message used to be read on the host
    /// every round, by any role holding `chat`, with a missing file answering
    /// "no such file" — an existence oracle for arbitrary paths. Now the text
    /// the model receives says an attachment was withheld and the file is
    /// never opened, while a real attachment (a file in the media root, which
    /// is where a connector's own downloads land) still arrives.
    ///
    /// Mutation: delete the `enforce_inbound` call at the dispatch seam and
    /// the secret's contents turn up in the request body below.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_inbound_marker_from_a_below_owner_sender_is_never_read() {
        let (home, _guard) = veles_core::testenv::scratch_home();
        let (_allowed, secret, contents) = media_fixture_at(home.path(), "inbound");
        let note = home.path().join("media").join("note.txt");
        let url = veles_core::providers::tests::sse_server_seq_capture(vec![sse_round("ok")]).await;
        let (core, mut rx) = budget_core_at(home.path(), &url.0, MEDIA_ACL);

        core.handle(acl_msg(
            Some("irc:alice"),
            &format!(
                "look at these\nMEDIA:{}\nMEDIA:{}",
                secret.display(),
                note.display()
            ),
        ))
        .await;
        let _ = next_reply(&mut rx);

        let sent = url.1.lock().unwrap().join("\n---\n");
        assert!(
            !sent.contains(&contents),
            "the secret was never read into a request: {}",
            sent.chars().take(400).collect::<String>()
        );
        assert!(
            !sent.contains(&secret.display().to_string()),
            "and its path never reached the model either"
        );
        assert!(
            sent.contains("attachment withheld"),
            "the model is told, so it can answer instead of guessing: {}",
            sent.chars().take(400).collect::<String>()
        );
        assert!(
            sent.contains("MEDIA-ROOT-CONTENT"),
            "a real attachment from the media root still arrives"
        );
    }

    // ── command aliases ──────────────────────────────────────────────

    /// A gateway core whose config declares one chat alias and one media
    /// alias, pointed at a capturing fixture provider.
    fn alias_core(
        base_url: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        let (tmp, guard) = veles_core::testenv::scratch_home();
        let (core, rx) = alias_core_at(tmp.path(), base_url);
        (core, rx, tmp, guard)
    }

    fn alias_core_at(
        dir: &std::path::Path,
        base_url: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
    ) {
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert("irc".to_string(), HashMap::from([(String::new(), out_tx)]));
        let cfg = veles_core::config::VelesConfig {
            model: veles_core::config::ModelSection {
                provider: Some("fixture".into()),
                default: Some("fixture-model".into()),
                ..Default::default()
            },
            custom_providers: vec![
                veles_core::config::ProviderEntry {
                    name: "fixture".into(),
                    base_url: base_url.to_string(),
                    api_mode: "chat_completions".into(),
                    ..Default::default()
                },
                veles_core::config::ProviderEntry {
                    name: "other".into(),
                    base_url: base_url.to_string(),
                    api_mode: "chat_completions".into(),
                    models: veles_core::config::ModelsSpec::List(vec!["fast-model".into()]),
                    ..Default::default()
                },
            ],
            command_aliases: vec![veles_core::config::CommandAlias {
                name: "fastly".into(),
                kind: Some("chat".into()),
                provider: "other".into(),
                model: "fast-model".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks: HookBus::new(None),
        });
        (core, out_rx)
    }

    fn live_sid(core: &GatewayCore) -> String {
        core.sessions
            .lock()
            .unwrap()
            .get("irc:#chan")
            .expect("session exists")
            .session_id
            .clone()
    }

    /// `/fastly <prompt>` must do FOUR things, and three of them are easy
    /// to get separately right and jointly wrong:
    ///
    ///   1. run the prompt — not the whole line with the alias word still
    ///      on the front;
    ///   2. in a NEW session;
    ///   3. on the alias's route, not the one the old session was using
    ///      (`frontends::fork` carries overrides forward, so an
    ///      implementation that forgets `set_model` looks correct until
    ///      you read the bill);
    ///   4. leave that new session BOUND, so the next ordinary mention
    ///      continues it. Without the rebind the alias mints a session
    ///      nobody ever returns to.
    #[tokio::test]
    async fn a_chat_alias_starts_a_new_session_on_its_own_route_and_keeps_it() {
        let (url, seen) = capturing_llm().await;
        let (core, _rx, _home, _guard) = alias_core(&url);

        // An ordinary turn first, so there is a session to replace.
        core.handle(acl_msg(Some("irc:owner"), "hello")).await;
        let first = live_sid(&core);

        core.handle(acl_msg(Some("irc:owner"), "/fastly summarise this"))
            .await;
        let after = live_sid(&core);
        assert_ne!(after, first, "the alias must mint a NEW session");

        let agent = core
            .sessions
            .lock()
            .unwrap()
            .get("irc:#chan")
            .expect("session")
            .clone();
        let ov = agent.overrides.read().unwrap().clone();
        assert_eq!(
            ov.model.as_deref(),
            Some("fast-model"),
            "the fresh session must be on the ALIAS route, not the inherited one"
        );
        assert_eq!(ov.provider.as_deref(), Some("other"));

        // The prompt that ran is the REST of the line.
        let reqs = seen.lock().unwrap().clone();
        let last = reqs.last().expect("a turn ran").clone();
        assert!(
            last.contains("summarise this"),
            "the alias's prompt must reach the model: {last}"
        );
        assert!(
            !last.contains("/fastly"),
            "and the alias word must not: {last}"
        );

        // The next ordinary mention continues the session the alias made.
        core.handle(acl_msg(Some("irc:owner"), "and again")).await;
        assert_eq!(
            live_sid(&core),
            after,
            "an alias that does not REBIND mints a session nobody returns to"
        );
    }

    /// Bare `/fastly` is "put me on this route" — a session, no turn. An
    /// empty prompt would otherwise bill for nothing.
    #[tokio::test]
    async fn a_bare_chat_alias_switches_route_without_spending_a_turn() {
        let (url, seen) = capturing_llm().await;
        let (core, _rx, _home, _guard) = alias_core(&url);
        core.handle(acl_msg(Some("irc:owner"), "hello")).await;
        let before = seen.lock().unwrap().len();

        core.handle(acl_msg(Some("irc:owner"), "/fastly")).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            before,
            "a bare alias must not run a turn"
        );
        let agent = core
            .sessions
            .lock()
            .unwrap()
            .get("irc:#chan")
            .expect("session")
            .clone();
        assert_eq!(
            agent.overrides.read().unwrap().model.as_deref(),
            Some("fast-model"),
            "but it must still switch the route"
        );
    }

    /// The alias mints a session and changes what the next turn costs.
    /// That is exactly the class of implicit action the notes doctrine
    /// says must be visible, and `/new` already sets the precedent by
    /// printing the id it made.
    #[tokio::test]
    async fn a_chat_alias_announces_the_session_and_the_route_it_chose() {
        let (url, _seen) = capturing_llm().await;
        let (core, mut rx, _home, _guard) = alias_core(&url);
        core.handle(acl_msg(Some("irc:owner"), "/fastly")).await;
        let mut lines = Vec::new();
        while let Ok(m) = rx.try_recv() {
            lines.push(m.text);
        }
        let note = lines
            .iter()
            .find(|l| l.starts_with("(new session "))
            .unwrap_or_else(|| panic!("nothing announced the switch: {lines:?}"));
        assert!(
            note.contains("fast-model") && note.contains("other"),
            "the note must name the route it put you on: {note}"
        );
    }

    /// An ordinary sentence whose first word happens to name an alias is
    /// an ordinary sentence.
    ///
    /// The gateway's command block runs over EVERY inbound line, not only
    /// over slash lines — so before `resolve_command`, "fastly is a CDN"
    /// minted a session, switched provider, and ran "is a CDN" as the
    /// prompt. The reply was a non-sequitur billed to a provider nobody
    /// chose, and nothing said why.
    #[tokio::test]
    async fn a_sentence_starting_with_an_alias_word_is_not_a_command() {
        let (url, seen) = capturing_llm().await;
        let (core, _rx, _home, _guard) = alias_core(&url);
        core.handle(acl_msg(Some("irc:owner"), "hello")).await;
        let before = live_sid(&core);

        core.handle(acl_msg(Some("irc:owner"), "fastly is a CDN"))
            .await;
        assert_eq!(
            live_sid(&core),
            before,
            "a bare word must not mint a session"
        );
        let agent = core
            .sessions
            .lock()
            .unwrap()
            .get("irc:#chan")
            .expect("session")
            .clone();
        assert_ne!(
            agent.overrides.read().unwrap().model.as_deref(),
            Some("fast-model"),
            "and it must not switch the route"
        );
        let last = seen.lock().unwrap().last().cloned().expect("a turn ran");
        assert!(
            last.contains("fastly is a CDN"),
            "the WHOLE sentence is the prompt: {last}"
        );
    }

    // ── a config edit reaches new sessions ───────────────────────────
    //
    // `configedit::effect_for("model")` has always classified `[model]`
    // as NewSessions, and `/defaults` tells the operator "applies to
    // new sessions" when it writes. Both were lies while the gateway
    // handed every new agent the boot-time `Arc`: the file was re-read
    // by nobody, and the only way to pick up a `/defaults model` edit
    // was to restart the process — which is exactly what the IRC log
    // that prompted this showed an operator being told to do.

    /// A gateway core booted from a real config FILE in a scratch home.
    ///
    /// `cfg_from_file: true` is the whole distinction: an in-memory
    /// config (every other test here) has no file to be live about and
    /// must keep exactly what it was handed.
    fn file_backed_core(
        toml_text: &str,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        std::path::PathBuf,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        let (home, guard) = veles_core::testenv::scratch_home();
        let path = veles_core::configedit::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, toml_text).unwrap();
        veles_core::config::invalidate_live();

        let dir = home.path().to_path_buf();
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert("irc".to_string(), HashMap::from([(String::new(), out_tx)]));
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        // The BOOT snapshot — what the gateway held before the edit.
        let cfg = veles_core::config::load().expect("the file just written parses");
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(cfg),
            cfg_from_file: true,
            hooks: HookBus::new(None),
        });
        (core, out_rx, path, home, guard)
    }

    /// Two routes, `first` and `second`, with `default` naming the one
    /// in force. The edit under test moves `[model]` from one to the
    /// other.
    fn two_providers(default: &str) -> String {
        format!(
            r#"
[model]
provider = "{default}"
default = "{default}-model"

[[custom_providers]]
name = "first"
base_url = "http://127.0.0.1:0/v1"
api_mode = "chat_completions"
models = ["first-model"]

[[custom_providers]]
name = "second"
base_url = "http://127.0.0.1:2/v1"
api_mode = "chat_completions"
models = ["second-model"]
"#
        )
    }

    fn ctx_for(who: &str, chan: &str) -> veles_core::acl::SenderContext {
        veles_core::acl::SenderContext {
            mask: Some(format!("{who}!~{who}@box.example")),
            network: Some("localtest".into()),
            channel: Some(chan.into()),
            platform: Some("irc".into()),
            chat_id: Some(chan.into()),
            direct: false,
            handle: None,
            post: None,
        }
    }

    #[tokio::test]
    async fn a_config_edit_reaches_the_next_session_without_a_restart() {
        let (core, _rx, path, _home, _guard) = file_backed_core(&two_providers("first"));

        // Two CHATS, not two senders in one chat: the fixture config has
        // no `[acl]` section, so the ACL is dormant and every sender
        // resolves to Owner — and owner-operated chats key their session
        // on the chat alone (`session_key_for`), which would collapse
        // "early" and "late" onto ONE agent and quietly test nothing.
        let before = core
            .agent_for_sender(
                "irc",
                "#early",
                Some("irc:early"),
                &ctx_for("early", "#early"),
            )
            .expect("session");
        assert_eq!(
            before.effective_model().0,
            "first",
            "sanity: the session before the edit is on the boot route"
        );

        // The edit. Written through the same path `/defaults` uses, so
        // this is the operator's action and not a hand-poked struct.
        veles_core::configedit::edit("model.provider", |d| {
            veles_core::configedit::set_value(d, &["model", "provider"], "second".into())?;
            veles_core::configedit::set_value(d, &["model", "default"], "second-model".into())
        })
        .expect("the edit lands");
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("second"),
            "it reached the file"
        );
        veles_core::config::invalidate_live();

        // NO RESTART. Same `GatewayCore`, same boot `Arc` in `core.cfg`.
        let after = core
            .agent_for_sender("irc", "#late", Some("irc:late"), &ctx_for("late", "#late"))
            .expect("session");
        let (provider, model, _, _) = after.effective_model();
        assert_eq!(
            (provider.as_str(), model.as_str()),
            ("second", "second-model"),
            "a session started AFTER the edit must use it — `NewSessions` \
             has to mean new sessions, not new processes"
        );
        assert_eq!(
            core.cfg.model.provider.as_deref(),
            Some("first"),
            "and the boot snapshot is genuinely still the old one, so the \
             assertion above cannot be passing by accident"
        );

        // The other half of the promise: an OPEN session keeps its route.
        assert_eq!(
            before.effective_model().0,
            "first",
            "a turn belongs to the config it started under"
        );
    }

    /// `/new` is a new session too. It goes through `frontends::fork`,
    /// which rebuilt on `old.cfg` — so the one command an operator uses
    /// to pick up a changed default was the one that could not.
    #[tokio::test]
    async fn a_new_session_after_an_edit_comes_up_on_the_new_default() {
        let (core, _rx, _path, _home, _guard) = file_backed_core(&two_providers("first"));
        let ctx = ctx_for("someone", "#chan");
        let old = core
            .agent_for_sender("irc", "#chan", Some("irc:someone"), &ctx)
            .expect("session");
        assert_eq!(old.effective_model().0, "first");

        veles_core::configedit::edit("model.provider", |d| {
            veles_core::configedit::set_value(d, &["model", "provider"], "second".into())?;
            veles_core::configedit::set_value(d, &["model", "default"], "second-model".into())
        })
        .expect("the edit lands");
        veles_core::config::invalidate_live();

        let fresh = core
            .rebind_session(
                "irc",
                "#chan",
                Some("irc:someone"),
                &ctx,
                "20260912_180000_ab12cd",
            )
            .await
            .expect("/new");
        assert_eq!(
            fresh.effective_model().0,
            "second",
            "`/new` mints a session, so it gets the config that is current \
             now, not the one the session it replaced was born under"
        );
    }

    /// A file that will not parse keeps the last GOOD config.
    ///
    /// `config::live()` answered a parse failure with
    /// `VelesConfig::default()` and cached that under the broken file's
    /// key. For the ACL that fails closed and merely refuses everything;
    /// for `[model]` it is "no model.provider in config", which is a
    /// session that cannot be created at all — and an editor's write is
    /// not atomic, so the window is ordinary rather than exotic.
    #[tokio::test]
    async fn a_broken_config_file_does_not_take_new_sessions_down_with_it() {
        let (core, _rx, path, _home, _guard) = file_backed_core(&two_providers("first"));
        let good = core
            .agent_for_sender("irc", "#one", Some("irc:one"), &ctx_for("one", "#one"))
            .expect("session");
        assert_eq!(good.effective_model().0, "first");

        // Half-written: a truncated table, the shape an interrupted
        // write actually leaves.
        std::fs::write(&path, "[model]\nprovider = \"second\"\ndefau").unwrap();
        veles_core::config::invalidate_live();

        let after = core
            .agent_for_sender("irc", "#two", Some("irc:two"), &ctx_for("two", "#two"))
            .expect("a broken file must not make sessions uncreatable");
        assert_eq!(
            after.effective_model().0,
            "first",
            "the last config that parsed is the one still in force"
        );
    }

    // ── [[model.binds]] ──────────────────────────────────────────────

    /// A gateway whose config binds everyone at or below `trusted` to a
    /// second provider. `irc:owner` is owner, `irc:friend` is trusted,
    /// and an unlisted nick is a guest.
    fn bind_core(
        binds: Vec<veles_core::config::ModelBind>,
    ) -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        // `scratch_home`, not a hand-built `set_var`: `Agent::new`
        // (inside `agent_for_sender`) writes a memory hierarchy into
        // VELES_HOME, and the old spelling leaked the env var for the
        // rest of the binary. The guard must live in the TEST BODY, not
        // this fixture's frame — see testenv.rs rule 2.
        let (tmp, guard) = veles_core::testenv::scratch_home();
        let dir = tmp.path().to_path_buf();
        let (in_tx, _in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = HashMap::new();
        outbound.insert("irc".to_string(), HashMap::from([(String::new(), out_tx)]));
        let cfg = veles_core::config::VelesConfig {
            acl: serde_yaml::from_str("owner: [\"irc:owner\"]\nroles: {\"irc:friend\": trusted}")
                .unwrap(),
            model: veles_core::config::ModelSection {
                provider: Some("main".into()),
                default: Some("main-model".into()),
                binds,
                ..Default::default()
            },
            custom_providers: vec![
                veles_core::config::ProviderEntry {
                    name: "main".into(),
                    base_url: "http://127.0.0.1:0/v1".into(),
                    api_mode: "chat_completions".into(),
                    ..Default::default()
                },
                veles_core::config::ProviderEntry {
                    name: "cheap".into(),
                    base_url: "http://127.0.0.1:2/v1".into(),
                    api_mode: "chat_completions".into(),
                    models: veles_core::config::ModelsSpec::List(vec!["cheap-model".into()]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.join("state.db")).unwrap(),
        ));
        let core = Arc::new(GatewayCore {
            bus: GatewayBus {
                inbound: in_tx,
                outbound: Arc::new(std::sync::Mutex::new(outbound)),
                last_chat: Arc::new(std::sync::Mutex::new(HashMap::new())),
                state_path: dir.join("gateway-state.json"),
            },
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store,
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks: HookBus::new(None),
        });
        (core, out_rx, tmp, guard)
    }

    /// The `/sudo` gateway: an ACL with one owner, one trusted friend and
    /// one TICKETED owner (`irc:tester`), and a scratch home holding a node
    /// id and one trusted signer key.
    fn ticket_core() -> (
        Arc<GatewayCore>,
        tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
        veles_core::ticket::SignerKey,
    ) {
        let (core, rx, tmp, guard) = bind_core(Vec::new());
        let mut cfg = (*core.cfg).clone();
        cfg.acl = toml::from_str(
            "owner = [\"irc:owner\"]\ndefault_role = \"guest\"\n\
             [roles]\n\"irc:friend\" = \"trusted\"\n\
             [[users]]\nname = \"tester\"\nrole = \"user\"\nticket_role = \"owner\"\n\
             identities = [\"irc:tester\"]\n",
        )
        .unwrap();
        let core = Arc::new(GatewayCore {
            bus: core.bus.clone(),
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store: core.store.clone(),
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks: HookBus::new(None),
        });
        let (key, _) = veles_core::ticket::SignerKey::generate().unwrap();
        std::fs::write(
            veles_core::config::config_dir().join("node.toml"),
            "id = \"sudonode00000001\"\nname = \"sudonode\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            format!(
                "[cluster.ticket_signers.test]\nkey = \"{}\"\n",
                key.public_config()
            ),
        )
        .unwrap();
        (core, rx, tmp, guard, key)
    }

    /// On a public feed `/sudo` passes the filter for a caller who holds a
    /// role by ticket — the raised command comes back through it as that
    /// role — and for nobody else, who is told the usual and no hint.
    #[test]
    fn sudo_passes_the_public_feed_filter_only_for_a_ticketed_caller() {
        let acl: veles_core::acl::AclConfig = toml::from_str(
            "default_role = \"guest\"\n[[users]]\nname = \"tester\"\nrole = \"user\"\n\
             ticket_role = \"owner\"\nidentities = [\"atproto:did:plc:tester\"]\n",
        )
        .unwrap();
        let t = veles_core::acl::Principal::resolve(Some("atproto:did:plc:tester"), &acl);
        assert!(public_feed_refusal("atproto", "/sudo /tcladmin puts 1", &t).is_none());
        let r = public_feed_refusal("atproto", "/acl list", &t).expect("refused standing");
        assert!(
            r.contains("/sudo"),
            "a ticketed caller is told how to ask: {r}"
        );
        let u =
            veles_core::acl::Principal::known("atproto:did:plc:other", veles_core::acl::Role::User);
        let r = public_feed_refusal("atproto", "/sudo /acl list", &u).expect("refused");
        assert!(!r.contains("by ticket"), "{r}");
    }

    fn from(who: &str, id: &str, text: &str) -> InboundMessage {
        InboundMessage {
            platform: "irc".into(),
            route: None,
            chat_id: "#chan".into(),
            msg_id: Some(id.into()),
            dedup_id: None,
            sender: who.into(),
            sender_id: Some(format!("irc:{who}")),
            sender_ctx: bind_ctx(who),
            text: text.into(),
            context: None,
        }
    }

    /// USER 2026-10-11: a ticketed owner "uses our ticketing to operate".
    /// A command above its standing level is refused with how to ask;
    /// `/sudo <command>` runs that one command at owner on a ticket for
    /// exactly it, pasted back in the chat; a decline runs nothing; a
    /// caller with no ticketed role and a prompt dressed as a command are
    /// refused without anyone being asked.
    #[tokio::test]
    async fn sudo_raises_one_command_on_a_signed_ticket() {
        let (core, mut rx, _home, _guard, key) = ticket_core();
        let next = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMessage>| {
            rx.try_recv().map(|m| m.text).unwrap_or_default()
        };

        // Standing level: refused, and told how to ask — at the gateway's
        // own command gate (a floor a user lacks: `/bg` is cmd-admin)…
        core.handle(from("tester", "m0", "/bg echo hi")).await;
        let r = next(&mut rx);
        assert!(
            r.contains("requires a higher access level") && r.contains("/sudo"),
            "{r}"
        );
        // …and inside a command whose own check needs more.
        core.handle(from("tester", "m1", "/acl list")).await;
        let r = next(&mut rx);
        assert!(r.contains("denied") && r.contains("/sudo"), "{r}");

        // No role held by ticket: `/sudo` is not a way in.
        core.handle(from("friend", "m2", "/sudo /acl list")).await;
        let r = next(&mut rx);
        assert!(r.contains("you hold none here"), "{r}");

        // A prompt is not a command.
        core.handle(from("tester", "m3", "/sudo read the owner's memory"))
            .await;
        let r = next(&mut rx);
        assert!(r.contains("raises one built-in command"), "{r}");

        // A command whose own check needs the owner says how to ask too.
        core.handle(from("tester", "m3b", "/tcladmin set x 1"))
            .await;
        let r = next(&mut rx);
        assert!(r.contains("denied") && r.contains("/sudo"), "{r}");

        // Signed: the ask names the exact command; the pasted ticket runs it.
        let c = core.clone();
        let task = veles_core::rt::spawn(async move {
            c.handle(from("tester", "m4", "/sudo /acl list")).await
        });
        let ask = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("the ticket is asked for")
            .expect("an outbound ask");
        assert_eq!(ask.kind, MessageKind::Ask, "{}", ask.text);
        let req = veles_core::ticket::TicketRequest::decode(
            ask.ask
                .as_ref()
                .and_then(|a| a.detail.as_deref())
                .unwrap_or(""),
        )
        .expect("the request rides the ask");
        assert_eq!(req.tool, veles_core::ticketed::SUDO_TOOL);
        assert_eq!(req.args, serde_json::json!({"command": "/acl list"}));
        assert_eq!(req.grant, "owner");
        let ticket = key.sign(&req).unwrap();
        assert!(
            veles_core::ask::resolve_for_chat("irc:#chan", &ticket, Some("user:tester")).is_some(),
            "the pasted ticket answers the ask"
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let r = next(&mut rx);
        // The listing itself (it reads the live file, which holds no
        // [acl] here) — what `/acl list` prints only past its gate.
        assert!(r.contains("runtime grants"), "the rules, as owner: {r}");
        assert!(!r.contains("denied"), "{r}");

        // Declined: nothing runs.
        let c = core.clone();
        let task = veles_core::rt::spawn(async move {
            c.handle(from("tester", "m5", "/sudo /acl list")).await
        });
        let ask = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ask.kind, MessageKind::Ask);
        assert!(
            veles_core::ask::resolve_for_chat("irc:#chan", "no", Some("user:tester")).is_some()
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let r = next(&mut rx);
        assert!(r.contains("did not run") && r.contains("declined"), "{r}");
    }

    /// A parked row handed back to the pipeline (a cron kick, a wake
    /// drain, a session-mail nudge) must land in the session it was
    /// parked in. On a configured ACL the empty sender it used to carry
    /// resolved as a stranger, keyed to ANOTHER session; the session's
    /// own sender resolves to the same key.
    #[test]
    fn a_parked_row_is_handed_back_as_the_sessions_own_sender() {
        let (core, _rx, _tmp, _guard) = bind_core(Vec::new());
        let mut cfg = (*core.cfg).clone();
        cfg.acl = toml::from_str("owner = [\"irc:owner\"]\ndefault_role = \"guest\"\n").unwrap();
        let core = Arc::new(GatewayCore {
            bus: core.bus.clone(),
            sessions: std::sync::Mutex::new(HashMap::new()),
            turn_locks: std::sync::Mutex::new(HashMap::new()),
            joins: std::sync::Mutex::new(HashMap::new()),
            progress: Arc::new(std::sync::Mutex::new(HashMap::new())),
            speakers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            turn_permits: Arc::new(Mutex::new(HashMap::new())),
            session_ids: std::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            store: core.store.clone(),
            cfg: Arc::new(cfg),
            cfg_from_file: false,
            hooks: HookBus::new(None),
        });
        let ctx = bind_ctx("owner");
        let agent = core
            .agent_for_sender("irc", "#chan", Some("irc:owner"), &ctx)
            .unwrap();
        let key = core.session_key_for("irc", "#chan", Some("irc:owner"), &ctx);
        let msg = core.parked_inbound(
            &agent.session_id,
            "irc".into(),
            "#chan".into(),
            &veles_core::store::QueueItem {
                session_id: agent.session_id.clone(),
                seq: 1,
                body: "x".into(),
                state: "pending".into(),
                kind: Some("message".into()),
                reply_to: Some("at://did:plc:op/app.bsky.feed.post/asked".into()),
                route: Some("IRC4Fun".into()),
                queued_at: String::new(),
            },
        );
        assert_eq!(
            core.session_key_for(
                &msg.platform,
                &msg.chat_id,
                msg.sender_id.as_deref(),
                &msg.sender_ctx
            ),
            key,
            "the handed-back row resolves to the session it was parked in"
        );
        // `msg_id` is the REAL recorded target, so a threaded adapter
        // answers under the message that asked. It used to be a
        // fabricated `queued:` id, which is not an at-uri: atproto's
        // `deliver_reply` could not resolve refs for it and refused to
        // post rather than dump the answer top-level on a public feed,
        // so the reply vanished with only the sender's mail row as a
        // trace (found live 2026-10-16).
        assert_eq!(
            msg.msg_id.as_deref(),
            Some("at://did:plc:op/app.bsky.feed.post/asked"),
            "the reply threads under the message that parked the row"
        );
        assert_eq!(
            msg.route.as_deref(),
            Some("IRC4Fun"),
            "the answer leaves on the adapter instance that carried the question"
        );
        // The synthetic id moved to `dedup_id`: the real one is already
        // claimed in the inbound ledger, so re-sending it bare would read
        // as a redelivery and the reply would be dropped as a duplicate.
        // The shape is `queued:<nanos>-<seq>`.
        let dedup = msg.dedup_id.as_deref().unwrap_or_default();
        assert!(
            dedup.starts_with("queued:") && dedup.ends_with("-1"),
            "the re-injection still gets its own exactly-once key: {dedup}"
        );
        assert_ne!(
            dedup_key(&msg),
            format!(
                "id:irc:#chan:{}",
                "at://did:plc:op/app.bsky.feed.post/asked"
            ),
            "the re-injection must NOT collide with the original claim"
        );
        assert_eq!(
            dedup_key(&msg),
            format!("id:irc:#chan:{dedup}"),
            "and it claims on the synthetic id, leaving the real one free"
        );
        // What it used to send lands somewhere else entirely.
        assert_ne!(
            core.session_key_for(
                "irc",
                "#chan",
                None,
                &veles_core::acl::SenderContext::default()
            ),
            key,
            "an empty sender is a stranger on a configured ACL"
        );
    }

    fn bind_ctx(who: &str) -> veles_core::acl::SenderContext {
        veles_core::acl::SenderContext {
            mask: Some(format!("{who}!~{who}@example.net")),
            network: Some("localtest".into()),
            channel: Some("#chan".into()),
            platform: Some("irc".into()),
            chat_id: Some("#chan".into()),
            direct: false,
            handle: None,
            post: None,
        }
    }

    /// The case the feature exists for, end to end: the channel lands on
    /// the cheap route without anybody typing `/model`, and the operator
    /// does not.
    #[tokio::test]
    async fn a_role_bound_route_catches_the_channel_and_leaves_the_owner_alone() {
        let (core, _rx, _home, _guard) = bind_core(vec![veles_core::config::ModelBind {
            platform: Some("irc".into()),
            role: Some("<=trusted".into()),
            provider: Some("cheap".into()),
            model: Some("cheap-model".into()),
            ..Default::default()
        }]);

        let guest = core
            .agent_for_sender("irc", "#chan", Some("irc:stranger"), &bind_ctx("stranger"))
            .expect("guest session");
        assert_eq!(
            guest.overrides.read().unwrap().model.as_deref(),
            Some("cheap-model"),
            "a guest must land on the bound route with nobody typing /model"
        );

        let friend = core
            .agent_for_sender("irc", "#chan", Some("irc:friend"), &bind_ctx("friend"))
            .expect("trusted session");
        assert_eq!(
            friend.overrides.read().unwrap().model.as_deref(),
            Some("cheap-model"),
            "trusted is inside `<=trusted`"
        );

        let owner = core
            .agent_for_sender("irc", "#chan", Some("irc:owner"), &bind_ctx("owner"))
            .expect("owner session");
        assert_eq!(
            owner.overrides.read().unwrap().model.as_deref(),
            None,
            "the owner is above the range and keeps [model] default"
        );
    }

    /// A bind FILLS an empty route and never overrides an explicit one.
    ///
    /// Driven against `apply_model_bind` DIRECTLY, and the first version
    /// of this test is the reason. It went through `agent_for_sender`
    /// twice and asserted the second call left the explicit choice
    /// alone — which is true, but true because the bind is only applied
    /// on the creation branch, so the guard never ran at all. Deleting
    /// the guard left that version green. Mutation testing earns its
    /// keep on exactly this: an assertion that holds for a reason other
    /// than the one it claims.
    ///
    /// The guard is a precondition of the function rather than of its
    /// current callers, and it is what makes the function safe to call
    /// after a fork — `frontends::fork` carries the previous session's
    /// overrides forward.
    #[tokio::test]
    async fn a_bind_fills_an_empty_route_and_never_overrides_an_explicit_one() {
        let (core, _rx, _home, _guard) = bind_core(vec![veles_core::config::ModelBind {
            platform: Some("irc".into()),
            provider: Some("cheap".into()),
            model: Some("cheap-model".into()),
            ..Default::default()
        }]);
        let ctx = bind_ctx("stranger");
        let a = core
            .agent_for_sender("irc", "#chan", Some("irc:stranger"), &ctx)
            .expect("session");
        assert_eq!(
            a.overrides.read().unwrap().model.as_deref(),
            Some("cheap-model"),
            "an empty route is filled"
        );

        // Now an explicit choice, and the bind applied straight at it.
        a.set_model("main-model@main").expect("explicit");
        let principal = core.resolve(Some("irc:stranger"), &ctx);
        core.apply_model_bind(&a, &principal, &ctx);
        assert_eq!(
            a.overrides.read().unwrap().model.as_deref(),
            Some("main-model"),
            "a bind must not stamp over a route somebody chose"
        );
    }

    /// No bind, no change — the mechanism is inert until configured.
    #[tokio::test]
    async fn with_no_binds_a_session_starts_on_the_plain_default() {
        let (core, _rx, _home, _guard) = bind_core(vec![]);
        let a = core
            .agent_for_sender("irc", "#chan", Some("irc:stranger"), &bind_ctx("stranger"))
            .expect("session");
        assert_eq!(a.overrides.read().unwrap().model.as_deref(), None);
    }

    /// The owner's telegram id was in `~/.hermes/config.yaml` under
    /// `platforms.telegram.home_channel.chat_id` and veles read it
    /// nowhere — `config_surface.rs` uses that very table as its worked
    /// example of a hermes block that survives an audit as DEAD config.
    /// USER: "my id should have already been there tbh, it was in
    /// hermes." So the hermes spelling is the live one, rather than a
    /// new key that would have stranded the old value a second time.
    /// `platforms` is a free-form blob, so the migration already carried
    /// it across; only the reader was missing.
    #[test]
    fn the_owners_home_chat_reads_hermes_own_spelling() {
        let cfg: veles_core::config::VelesConfig = toml::from_str(
            r#"
[platforms.telegram]
enabled = true

[platforms.telegram.home_channel]
platform = "telegram"
chat_id = "123456789"
"#,
        )
        .expect("parses");
        assert_eq!(
            super::owner_home_chat(&cfg),
            Some(("telegram".into(), "123456789".into()))
        );

        // A YAML-shaped NUMBER is the same destination. hermes quoted
        // it; a hand-edited toml very plausibly will not, and a chat id
        // that silently fails to resolve is the worst kind of "I turned
        // it on and nothing happened".
        let numeric: veles_core::config::VelesConfig =
            toml::from_str("[platforms.telegram.home_channel]\nchat_id = 123456789\n")
                .expect("parses");
        assert_eq!(
            super::owner_home_chat(&numeric),
            Some(("telegram".into(), "123456789".into()))
        );

        // No block, or a blank value: no destination, and the caller
        // says so rather than installing a relay that reaches nobody.
        let none: veles_core::config::VelesConfig = toml::from_str("").expect("parses");
        assert!(super::owner_home_chat(&none).is_none());
        let blank: veles_core::config::VelesConfig =
            toml::from_str("[platforms.telegram.home_channel]\nchat_id = \"  \"\n")
                .expect("parses");
        assert!(super::owner_home_chat(&blank).is_none());
    }

    /// A sync tick must apply the automatic inboxes before asking about
    /// them. `syncask::ask_due_with` repeats this boundary itself, so an
    /// offer arriving between the two calls is covered too.
    #[test]
    fn sync_drains_run_before_owner_decisions() {
        let src = include_str!("gateway.rs");
        let drain = src
            .find("drain_sync_inboxes(&store, cfg.cluster.configsync_auto")
            .expect("the tick calls its drain");
        let ask = src[drain..]
            .find("veles_core::syncask::ask_due")
            .map(|n| drain + n)
            .expect("the tick asks after the drain");
        assert!(drain < ask);
        let core = include_str!("../../veles-core/src/syncask.rs");
        let entry = core
            .find("pub fn ask_due_with(")
            .expect("the core decision entry exists");
        let body = &core[entry..];
        assert!(
            body.find("drain_automatic(").unwrap() < body.find("let pending =").unwrap(),
            "the core drains before its first scan as well"
        );
    }
}
