//! The ask channel — one way for the engine to ask the human a question
//! and block on the answer, on every frontend.
//!
//! Two consumers share it: the approval gate (`approval.rs`) and the
//! `clarify` tool. The shapes differ only in `AskKind`.
//!
//! Why a process-global pending registry rather than a plain oneshot
//! handed to the frontend: on chat platforms the ANSWER arrives through a
//! completely different path from the question (a telegram callback
//! query, an IRC PRIVMSG, an HTTP POST from the Tk app), often in another
//! task, and it must NOT be routed through the gateway's inbound queue —
//! that queue's turn for this chat is exactly what's parked waiting for
//! it. Adapters resolve asks directly here and never call `send_inbound`
//! for an answer. Registry shape follows `tools::EXTRA_TOOLS` (OnceLock +
//! Mutex) and the MCP pending-request map.
//!
//! Every ask has a hard timeout. A human who wanders off must not pin a
//! turn (and, on the gateway, a chat lock) forever.

use crate::lockutil::{LockOk, RwLockOk};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub use veles_types::ask::*;

/// Anything that can put a question in front of the human running this
/// session. Implementations live next to their frontend.
#[async_trait]
pub trait Asker: Send + Sync {
    async fn ask(&self, req: AskRequest) -> AskAnswer;
    /// False for `PolicyAsker` — callers use it to phrase things
    /// differently when nobody can possibly answer.
    fn interactive(&self) -> bool {
        true
    }
    /// May questions born on OTHER surfaces be relayed to this one?
    /// False for shared IRC channels (USER 2026-09-14: "it would be
    /// weird to have everything advertised to irc though with other
    /// people in the channel"). The HEAD leg — the surface a session's
    /// own question registers through — is never subject to this: an
    /// IRC-born session still asks in its own channel.
    fn wants_relay(&self) -> bool {
        true
    }
    /// Can this surface take a secret privately — a window or a
    /// terminal on the operator's own machine, not a chat other people
    /// (or a chat service) can read? Only such surfaces are shown a
    /// [`AskKind::Secret`] ask.
    fn private_input(&self) -> bool {
        false
    }
}

// ── pending registry ─────────────────────────────────────────────────

struct Pending {
    req: AskRequest,
    chat_key: Option<String>,
    /// Verified sender of the session that opened this ask
    /// (`platform:principal`). Answers from OTHER senders in the same
    /// chat are refused (S15 ACL) unless they resolve as owner.
    sender_id: Option<String>,
    /// One leg PER SURFACE that has rendered this ask, each with its
    /// own drop id. The ask id is the question's identity, not any one
    /// channel's: the same ask is visible on the desktop (bus + REST)
    /// and in a chat (adapter message) at the same time, and whichever
    /// surface's human answers FIRST wins — `resolve` delivers to every
    /// leg, so the losing surfaces' dialogs and prompts close on their
    /// own without a coordinator. `len() > 1` is the relayed state; a
    /// late leg can join while the ask is open (reattach, second
    /// window). A leg that fails to deliver drops ONLY itself
    /// (`drop_leg`) — one surface's dead adapter must not cancel the
    /// question out from the surfaces that rendered it fine.
    txs: Vec<(u64, tokio::sync::oneshot::Sender<AskAnswer>)>,
    seq: u64,
}

static PENDING: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static LEG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Ask ids are SINGLE-SHOT: once resolved/cancelled/timed out, an id is
/// never resurrected. A surface that joins between the resolve and its
/// own render (the reattach race) gets `None` from `register` and must
/// treat the ask as already answered — without this, the client polled
/// `GET /api/asks`, the ask resolved on another surface a moment later,
/// and the reattach REGISTERED A ZOMBIE nobody would ever consume,
/// which then sat in every asks list until its timeout.
static TOMBSTONES: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

fn tombstones() -> &'static Mutex<std::collections::HashSet<String>> {
    TOMBSTONES.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One rendered copy of an ask, on one surface. Await the answer with
/// [`AskLeg::wait`]; abandon (this surface failed/closed) with
/// [`AskLeg::drop_self`] — never with `cancel`, which would end the
/// question for EVERY surface. Dropping the leg is drop-self too (see
/// the `Drop` impl) — cancellation of a parked `ask()` future ends its
/// leg the same way an explicit abandon does.
pub struct AskLeg {
    pub req: AskRequest,
    pub leg: u64,
    rx: Option<tokio::sync::oneshot::Receiver<AskAnswer>>,
}

impl AskLeg {
    /// Await an answer with the request's timeout. On timeout the WHOLE
    /// ask ends (all legs share the request's deadline, so the others
    /// are at theirs too) — this is the one leg-scoped path allowed to
    /// end the shared ask.
    pub async fn wait(mut self) -> AskAnswer {
        let out = match tokio::time::timeout(
            std::time::Duration::from_secs(self.req.timeout_secs),
            self.rx.take().expect("wait consumes the receiver once"),
        )
        .await
        {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => {
                // Sender dropped: every other leg already got the
                // answer (resolve) or dropped too (all legs abandoned).
                AskAnswer::timeout()
            }
            Err(_) => {
                cancel(&self.req.id);
                tracing::info!(
                    "ask {} timed out after {}s",
                    self.req.id,
                    self.req.timeout_secs
                );
                AskAnswer::timeout()
            }
        };
        out
        // `self` drops HERE with `rx == None`: the Drop impl runs
        // drop-leg, which is a no-op the second time (the leg row is
        // already gone — resolved, cancelled, or timed out) and exactly
        // the right cleanup when it is not.
    }

    /// This surface cannot render the ask (adapter gone, channel dead).
    /// Drops ONLY this leg; the ask stays open on every other surface,
    /// and ends here if this was the last leg.
    pub fn drop_self(self) {
        // Body is the Drop impl; spelled out as a method because the
        // CALL is the documentation at the sites that choose it.
    }
}

/// Dropping an [`AskLeg`] is drop-self: removes this leg's sender from
/// the pending ask and ends the ask (tombstone) when no legs remain.
///
/// A leg lives as a local across an `await` inside its asker's
/// `ask()` future; when that future is cancelled (a bystander relay
/// task whose surface detached mid-park, the cancellation-on-detach
/// path in `ask_session`), no code of ours runs to call
/// `drop_self` — the drop of the future's locals is the one moment
/// that can end the leg. Without this, a cancelled relay left a leg
/// whose receiver was gone but whose sender still counted: the ask's
/// leg list said a surface was listening that no longer was (the
/// bounded-zombie half PENDING recorded as "cancellation-on-detach
/// owed").
///
/// Idempotent: `drop_leg` is a `retain` that matches nothing once the
/// row is gone, so `wait()`'s tail-of-function drop and any explicit
/// `drop_self` are unaffected.
impl Drop for AskLeg {
    fn drop(&mut self) {
        drop_leg(&self.req.id, self.leg);
    }
}

/// Remove one leg by drop id. Ends the ask (tombstone) when no legs
/// remain, so it cannot be rejoined by a late surface.
fn drop_leg(id: &str, leg: u64) {
    let mut map = pending().lock_ok();
    if let Some(p) = map.get_mut(id) {
        p.txs.retain(|(l, _)| *l != leg);
        if p.txs.is_empty() {
            map.remove(id);
            tombstones().lock_ok().insert(id.to_string());
        }
    }
}

/// Publish an ask and get the leg its answer will arrive on.
/// `chat_key` ("telegram:123") lets a chat adapter resolve by chat when
/// the platform can't echo the ask id back. `sender_id` binds WHO may
/// answer (the asker's verified identity); None = owner surface.
///
/// Calling this for an ask id that is ALREADY open attaches a NEW leg
/// to it (the relay path). A chat-keyed joining leg TIGHTENS the ask's
/// bindings to its own (chat, sender): the ask becomes answerable in
/// that chat only by that sender or an owner — without this, a
/// desktop-headed ask relayed into a channel was answerable by any
/// channel member typing "1" (`(None, None)` reads as owner-surface).
/// `None` = the ask already ended (answered, cancelled, timed out);
/// do not render it.
pub fn register(
    req: &AskRequest,
    chat_key: Option<&str>,
    sender_id: Option<&str>,
) -> Option<AskLeg> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let leg = LEG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut map = pending().lock_ok();
    if tombstones().lock_ok().contains(&req.id) {
        return None;
    }
    match map.get_mut(&req.id) {
        // Relay: same question, another surface. The seq stays the
        // ORIGINAL's, so ordering across asks is stable no matter when
        // a leg joined.
        Some(p) => {
            if p.chat_key.is_none() {
                if let Some(k) = chat_key {
                    p.chat_key = Some(k.to_string());
                    p.sender_id = sender_id.map(String::from);
                }
            }
            p.txs.push((leg, tx));
        }
        None => {
            map.insert(
                req.id.clone(),
                Pending {
                    req: req.clone(),
                    chat_key: chat_key.map(String::from),
                    sender_id: sender_id.map(String::from),
                    txs: vec![(leg, tx)],
                    seq,
                },
            );
        }
    }
    Some(AskLeg {
        req: req.clone(),
        leg,
        rx: Some(rx),
    })
}

/// Deliver an answer. Every leg gets it — on relayed asks the surfaces
/// that did not win still need to close their dialogs/prompts, and the
/// answer itself is the signal ("someone answered elsewhere"). Returns
/// false when the ask is unknown (already answered, or timed out) —
/// callers should tell the user so.
/// Deliver an answer. Every leg gets it — on relayed asks the surfaces
/// whose human answered SECOND learn about it here: `ask-resolved` on
/// the session tap is what closes their dialogs ("answered elsewhere"),
/// because their `ask` future returning is invisible to a surface that
/// renders from the event stream, not from the future.
pub fn resolve(id: &str, answer: AskAnswer) -> bool {
    let p = pending().lock_ok().remove(id);
    tombstones().lock_ok().insert(id.to_string());
    match p {
        Some(p) => {
            // A secret ask's answer is a choice and nothing else. Free
            // text is dropped here, before any leg or the bus sees it:
            // an operator who typed the value into a generic prompt
            // must not have it published as "the answer".
            let answer = if p.req.kind == AskKind::Secret {
                AskAnswer {
                    free_text: None,
                    per_question: Default::default(),
                    ..answer
                }
            } else {
                answer
            };
            let mut delivered = false;
            for (_, tx) in p.txs {
                // A leg whose receiver was dropped (surface gone) is
                // fine — the ask still resolves for the others.
                delivered |= tx.send(answer.clone()).is_ok();
            }
            crate::session_tap::publish(
                &p.req.session_id,
                "ask-resolved",
                json!({
                    "id": p.req.id,
                    "by": "answer",
                    "answer": answer.describe(),
                }),
            );
            delivered
        }
        None => false,
    }
}

/// Drop an ask without answering (turn cancelled).
pub fn cancel(id: &str) {
    if let Some(p) = pending().lock_ok().remove(id) {
        crate::session_tap::publish(
            &p.req.session_id,
            "ask-resolved",
            json!({ "id": p.req.id, "by": "cancel" }),
        );
    }
    tombstones().lock_ok().insert(id.to_string());
}

/// Open asks for one chat, oldest first.
pub fn open_for_chat(chat_key: &str) -> Vec<AskRequest> {
    let map = pending().lock_ok();
    let mut v: Vec<(&u64, &AskRequest)> = map
        .values()
        .filter(|p| p.chat_key.as_deref() == Some(chat_key))
        .map(|p| (&p.seq, &p.req))
        .collect();
    v.sort_by_key(|(s, _)| **s);
    v.into_iter().map(|(_, r)| r.clone()).collect()
}

/// Who is trying to decide an ask.
///
/// The two cases are NOT the same absence, which is why this is an
/// enum and not an `Option`. A connector that cannot name its sender
/// is a fail-closed case — on a services-less network that used to be
/// every ask in every channel, and a stranger typing "1" approved a
/// tool call. A person at the local keyboard is the opposite: they are
/// who the whole ACL exists to protect. Both once spelled themselves
/// `None`.
#[derive(Debug, Clone, Copy)]
pub enum Decider<'a> {
    /// A local owner surface: the REPL, the TUI, the desktop, the CLI.
    LocalOwner,
    /// A connector sender, by subject — `None` when the platform could
    /// not identify them at all.
    ///
    /// `owner` is resolved BY THE CALLER, not here. The owner override
    /// used to call the ctx-less `Principal::resolve(subject, cfg)`,
    /// which sees no `SenderContext` and therefore no mask rules — so
    /// on a services-less network, where the mask IS the identity for
    /// everyone, a mask-derived owner was never recognised as owner. It
    /// failed CLOSED (they could not decide someone else's ask, rather
    /// than strangers deciding theirs), but `/acl` said owner and this
    /// did not, and nothing pinned it either way.
    ///
    /// Every caller already holds the fully resolved principal, mask
    /// and all. Passing the answer is strictly better than re-deriving
    /// it from a subject string that cannot carry it.
    Sender {
        subject: Option<&'a str>,
        owner: bool,
    },
}

impl<'a> Decider<'a> {
    /// A sender whose ownerness has not been resolved. Answers bind to
    /// the subject and nothing else — no override.
    pub fn sender(subject: Option<&'a str>) -> Self {
        Decider::Sender {
            subject,
            owner: false,
        }
    }

    /// A sender with their resolved principal, so the owner override
    /// can fire for a mask-derived owner too.
    pub fn principal(p: &'a crate::acl::Principal) -> Self {
        Decider::Sender {
            subject: Some(p.subject.as_str()),
            owner: p.has(crate::acl::Cap::All),
        }
    }
}

/// May this decider answer this ask?
///
/// Extracted from `resolve_for_chat` when the explicit-id verbs
/// (`/answer <id>`, `/approve <id>`) became first-class commands: those
/// went straight to `resolve()` with no binding check at all, so a
/// second holder of `Cap::Approve` in a shared channel could decide
/// someone else's approval by naming its id — the same hole S15 closed
/// for a bare "1", reachable by the longer spelling.
pub fn may_answer(id: &str, who: Decider) -> bool {
    let map = pending().lock_ok();
    let Some(p) = map.get(id) else {
        return false; // resolved meanwhile, or never existed
    };
    let (responder, resolved_owner) = match who {
        Decider::LocalOwner => return true,
        Decider::Sender { subject, owner } => (subject, owner),
    };
    // The caller's answer first (it can see masks); the ctx-less
    // resolution stays as a fallback for callers that only have a
    // subject string, where it still recognises an EXACT-id owner.
    let is_owner = resolved_owner
        || crate::acl::Principal::resolve(responder, &acl_snapshot()).has(crate::acl::Cap::All);
    match (&p.sender_id, &p.chat_key) {
        // Bound to a sender: that sender, or an owner.
        (Some(owner_of_ask), _) => is_owner || Some(owner_of_ask.as_str()) == responder,
        // No sender AND no chat: an owner-surface ask
        // (REPL/TUI/desktop) — the old behavior, preserved.
        (None, None) => true,
        // No sender but bound to a CHAT: the platform could not
        // identify whoever opened it. That used to mean "anybody in the
        // channel may answer", which on a services-less network was
        // EVERY ask — a stranger could approve a tool call by typing
        // "1". Only an owner decides an ask nobody can be shown to own.
        (None, Some(_)) => is_owner,
    }
}

/// Try to interpret `text` as the answer to this chat's oldest open ask.
/// Returns the ask id when it resolved one — the adapter then swallows
/// the message instead of forwarding it as a new user turn.
///
/// `responder` = the verified sender id of whoever typed the reply
/// (None when the platform can't tell). An ask bound to a sender can
/// only be answered by that sender or an owner — a guest typing "1"
/// must not decide the owner's approval (S15 ACL).
pub fn resolve_for_chat(chat_key: &str, text: &str, responder: Option<&str>) -> Option<String> {
    let candidates = open_for_chat(chat_key);
    let req = candidates
        .into_iter()
        .find(|req| may_answer(&req.id, Decider::sender(responder)))?;
    let answer = parse_reply(&req, text)?;
    if resolve(&req.id, answer) {
        Some(req.id)
    } else {
        None
    }
}

// ── steer routing ────────────────────────────────────────────────────
//
// `/steer` and the desktop's steer endpoint used to write whatever
// `pending_steer` Arc their OWN agent cache handed them — and the
// gateway process has TWO caches keyed by the same session id
// (DesktopServer::agents and GatewayCore::sessions). A steer accepted
// by the "wrong" one queued into an Option nobody read: HTTP 200
// "queued" over a silent loss (PENDING 2026-09-14). Routing was never
// unified the way liveness was — `turn_is_live` was.
//
// ONE process-global slot per session is the fix: `run_turn` writes it
// at entry (the running turn's own pending_steer), so every surface
// steers the turn that is ACTUALLY running, whichever cache built the
// agent. When no slot is live the caller parks the steer in the
// session's durable queue (a `steer` row the next boundary injects) —
// a steer that cannot reach the current turn is a message that must
// not vanish, not an error.

/// The steer handle: one pending steer per running turn, shared with
/// every surface that may steer it (an `Agent::pending_steer` is one).
pub type SteerHandle = Arc<Mutex<Option<String>>>;

/// The running turn's steer handle, keyed by session id.
static STEER_SLOTS: OnceLock<Mutex<HashMap<String, SteerHandle>>> = OnceLock::new();

fn steer_slots() -> &'static Mutex<HashMap<String, SteerHandle>> {
    STEER_SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Claim the session's steer slot for the duration of a turn: publish
/// the turn's OWN `pending_steer` Arc as the process-wide steering
/// target, so every surface's `steer` write and this turn's
/// `take` drain the SAME Option regardless of which agent cache built
/// the agent. Hold the returned guard for the turn's life.
pub fn claim_steer_slot(session_id: &str, handle: SteerHandle) -> SteerSlotGuard {
    let guard = SteerSlotGuard {
        session_id: session_id.to_string(),
        handle: Some(handle.clone()),
    };
    steer_slots()
        .lock_ok()
        .insert(session_id.to_string(), handle);
    guard
}

/// RAII: clears the session's steer slot when the turn ends — but only
/// if the slot still points at OUR handle. Overlapping lifecycles on
/// one session id (which the queue mostly serializes, but a gateway
/// turn and a desktop turn can race) mean a second claim may have
/// replaced ours; deleting theirs on our exit would be the same
/// one-per-session-id bug this whole file is fixing. `handle: None`
/// marks a claim that was replaced (or a test claim) and drops clean.
pub struct SteerSlotGuard {
    session_id: String,
    handle: Option<SteerHandle>,
}

impl SteerSlotGuard {
    /// The handle steers are currently written into. If another turn
    /// replaced our claim, this is THEIR handle (routing to the live
    /// turn) — the honest answer for a surface steering mid-overlap.
    pub fn handle(&self) -> SteerHandle {
        steer_slots()
            .lock_ok()
            .get(&self.session_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(Mutex::new(None)))
    }
}

impl Drop for SteerSlotGuard {
    fn drop(&mut self) {
        if let Some(ours) = &self.handle {
            let mut slots = steer_slots().lock_ok();
            if let Some(cur) = slots.get(&self.session_id) {
                if Arc::ptr_eq(cur, ours) {
                    slots.remove(&self.session_id);
                }
            }
        }
    }
}

/// Hand a steer to the RUNNING turn, or park it for the next turn when
/// none is live. `Steered::Live` = the running turn's handle took it
/// (injects at its next tool round); `Steered::Parked` = it sits in the
/// process-global dock and the NEXT turn on this session start-injects
/// it (labeled `[STEER]`), however that turn is started and whichever
/// surface starts it. A parked steer survives frontend crashes, agent
/// cache evictions and config reloads — everything but a gateway
/// restart, which today's `pending_steer` never survived either.
///
/// Callers must surface WHICH one happened — a steer that parks behind
/// a compaction the user believes is a turn is a UX hazard the client
/// has to be able to name.
#[derive(Debug, PartialEq, Eq)]
pub enum Steered {
    Live,
    /// Live, AND a previous steer was still sitting undrained — this
    /// one was appended behind it rather than replacing it. `behind`
    /// counts the messages already waiting. A separate variant because
    /// the caller has to be able to SAY so: a steer that lands in the
    /// same tool-round gap as another is not the same event as one that
    /// lands alone, and silently merging them is the kind of quiet
    /// substitution this module exists to stop.
    LiveQueued {
        behind: usize,
    },
    Parked,
}

/// The separator inside one pending steer. The turn injects the slot as
/// a single `[STEER]` message, so two redirects arrive as two lines of
/// one — which is how a human who typed them a second apart reads them.
const STEER_JOIN: &str = "\n";

pub fn steer(session_id: &str, msg: &str) -> Steered {
    if let Some(h) = steer_slots().lock_ok().get(session_id).cloned() {
        // APPEND, do not replace. `Option::replace` returned the old
        // value and threw it away, so two steers arriving in the same
        // gap between tool rounds — a correction typed right after a
        // redirect, the common case — silently lost the first. That is
        // precisely the "a steer that cannot reach the current turn is
        // a message that must not vanish" rule, broken on the very
        // path it was written to protect.
        let mut slot = h.lock_ok();
        match slot.as_mut() {
            Some(existing) => {
                let behind = existing.split(STEER_JOIN).count();
                existing.push_str(STEER_JOIN);
                existing.push_str(msg);
                Steered::LiveQueued { behind }
            }
            None => {
                *slot = Some(msg.to_string());
                Steered::Live
            }
        }
    } else {
        dock_steer(session_id, msg);
        Steered::Parked
    }
}

/// Parked steers, keyed by session id — the fallback dock when no
/// store is reachable (tests, headless one-shots). The PRIMARY dock is
/// a `session_queue` kind='steer' row: durable across restarts, listed
/// by every surface's queue view, deleted with the session. The
/// in-memory Vec survives only within one process and only when nobody
/// could write the row.
static DOCKED_STEERS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();

fn docked_steers() -> &'static Mutex<HashMap<String, Vec<String>>> {
    DOCKED_STEERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The store the durable dock writes to: the process's shared `Store`,
/// registered by the surface that owns it — the gateway, when it parks
/// its core (`set_global_core`). None in unit tests and headless CLI
/// runs, which use the in-memory dock. (Until 2026-09-28 this read the
/// gateway's core directly; the gateway is its own crate now.)
static STEER_STORE: OnceLock<std::sync::RwLock<Option<Arc<Mutex<crate::store::Store>>>>> =
    OnceLock::new();

pub fn set_steer_store(store: Arc<Mutex<crate::store::Store>>) {
    if let Ok(mut w) = STEER_STORE
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
    {
        *w = Some(store);
    }
}

fn steer_store() -> Option<Arc<Mutex<crate::store::Store>>> {
    STEER_STORE.get()?.read().ok()?.clone()
}

fn dock_steer(session_id: &str, msg: &str) {
    // Durable first: one row in the store the whole process shares.
    // The gateway's core is parked process-wide; the desktop's server
    // was handed the SAME store object at boot, so either route writes
    // the row every surface reads.
    if let Some(store) = steer_store() {
        let store = store.lock_ok();
        if store.queue_push_kind(session_id, msg, "steer").is_ok() {
            return;
        }
    }
    // No store (unit tests, headless CLI): the in-memory dock, which
    // `take_docked_steers` drains within this process's lifetime.
    docked_steers()
        .lock_ok()
        .entry(session_id.to_string())
        .or_default()
        .push(msg.to_string());
}

/// Take every docked steer for this session, oldest first. Called at
/// turn START (before the first LLM call), so a steer parked while the
/// session was idle reaches the model as the turn's first redirect —
/// not never. Drains BOTH docks: the durable store rows first (they
/// may have been parked by a previous PROCESS), then whatever the
/// in-memory fallback holds.
pub fn take_docked_steers(session_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(store) = steer_store() {
        if let Ok(rows) = store.lock_ok().take_steers(session_id) {
            out.extend(rows);
        }
    }
    out.extend(
        docked_steers()
            .lock_ok()
            .remove(session_id)
            .unwrap_or_default(),
    );
    out
}

/// Drop a session's docked steers (session deleted). There is nothing
/// for them to steer any more.
pub fn discard_docked_steers(session_id: &str) {
    docked_steers().lock_ok().remove(session_id);
    // The durable dock rides the session delete in the store
    // (`DELETE FROM session_queue WHERE session_id`), so nothing more
    // is needed here when a store exists — this sweep is the fallback.
    if let Some(store) = steer_store() {
        let _ = store
            .lock_ok()
            .take_steers(session_id)
            .map(|v: Vec<String>| v.len());
    }
}

/// Is a turn's steer slot live right now? (Steering routes to the
/// running turn when one exists; otherwise it parks in the dock.)
pub fn steer_slot_live(session_id: &str) -> bool {
    steer_slots().lock_ok().contains_key(session_id)
}

// ── ACL snapshot for answer binding ──────────────────────────────────
//
// Loading config per answer resolution is wasteful and makes the ask
// layer depend on ambient VELES_HOME state. The ACL changes only on
// config edit + gateway restart, so a lazily-initialized process
// snapshot is the right shape (tests can override it explicitly).

static ACL_SNAPSHOT: std::sync::OnceLock<std::sync::RwLock<Option<crate::acl::AclConfig>>> =
    std::sync::OnceLock::new();

fn acl_snapshot_slot() -> &'static std::sync::RwLock<Option<crate::acl::AclConfig>> {
    ACL_SNAPSHOT.get_or_init(|| std::sync::RwLock::new(None))
}

fn acl_snapshot() -> crate::acl::AclConfig {
    {
        let g = acl_snapshot_slot().read_ok();
        if let Some(c) = g.as_ref() {
            return c.clone();
        }
    }
    // No override: read the LIVE section. The slot used to be filled
    // here with the first load and never invalidated, on the reasoning
    // that "the ACL changes only on config edit + gateway restart" —
    // which stopped being true the moment `[acl]` became genuinely
    // hot-reloaded (acl::live_config). A revoked sender must not still
    // be able to answer approvals.
    crate::acl::live_config()
}

/// Test/ops hook: override the ACL snapshot used for answer binding.
pub fn set_acl_snapshot_for_tests(cfg: Option<crate::acl::AclConfig>) {
    *acl_snapshot_slot().write_ok() = cfg;
}

// ── per-session askers ───────────────────────────────────────────────
//
// Tools (`clarify`) are built once per session but the surface that can
// answer may change mid-session — the desktop installs a fresh asker per
// request, bound to that request's SSE stream. So tools look the asker up
// by session id at call time instead of holding one.
//
// A session can have MORE THAN ONE live asker, and that is not an error
// state — it is the normal shape of a gateway process, where a desktop
// window and a chat platform both hold the same session id. This map
// used to be ONE slot per session id with install/remove by id, which
// meant any second lifecycle's cleanup ("the turn that installed me is
// over") erased the FIRST surface's route to its human. The incident
// (PENDING 2026-09-14, session 20260913_194936_559209): a desktop turn
// ended, its `clear_asker` emptied the slot the gateway's standing
// `ChatAsker` had been using, and every later `clarify` on that session
// answered itself with "no interactive user is available" — the agent
// literally forgot it had an owner.
//
// The shape now: a per-session list of ATTACHED askers, most recent
// first. A surface attaches (`Agent::set_asker`) when it takes over a
// session and detaches ONLY by the token it got (`Weak` handles the
// rest — the owner keeps the Arc). Removal by session id does not
// exist outside test teardown. The HEAD of the list is the ask the
// question registers through; every OTHER live leg is relayed the same
// ask (see `ask_session`), and because answers fan out to every leg of
// one ask id, whichever surface's human answers first closes the
// dialogs on all the others.

static SESSION_ASKERS: OnceLock<Mutex<HashMap<String, AskerSet>>> = OnceLock::new();

/// One attached asker: its registry slot IS this triple.
pub(crate) struct AttachedAsker {
    /// The ONLY thing that can detach it.
    pub id: AskerId,
    /// Liveness contract: the surface that attached keeps the strong
    /// `Arc`; this side sees it die when the surface goes away.
    pub weak: std::sync::Weak<dyn Asker>,
    /// Fired by [`detach_session_asker`]. A bystander relay task parked
    /// in this surface's `ask().await` `select!`s on this handle: detach
    /// is the one moment the surface KNOWS it is going away, and the
    /// parked future is the one thing the detach path could not reach.
    /// Without it the relay held a strong `Arc` for the whole await, the
    /// registry's `Weak` never went dead while the ask was open, and a
    /// surface that left still counted as attached — re-relayed to on
    /// the next ask until the question resolved (the bounded zombie,
    /// PENDING 2026-09-15 audit round).
    pub detached: Arc<tokio::sync::Notify>,
}

pub(crate) struct AskerSet {
    /// Attached askers, most recent LAST. Pruned of dead Weaks on
    /// every read.
    pub attached: Vec<AttachedAsker>,
    /// Strong refs held by the back-compat shim (`set_session_asker`),
    /// by token. Real attachments never land here: their owner keeps
    /// the Arc (the `Agent` field, the adapter's handle), and the Weak
    /// is the liveness contract. The shim's spelling ("install and
    /// walk away") has no owner — without a pin here the entry died
    /// inside the install, and the whole test fixture class (standing
    /// ChannelAsker awaits a question that never comes) collapsed.
    shim_pins: Vec<(AskerId, Arc<dyn Asker>)>,
}

/// Identity of an attachment: the ONLY thing that can detach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AskerId(u64);

fn session_askers_map() -> &'static Mutex<HashMap<String, AskerSet>> {
    SESSION_ASKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prune(set: &mut AskerSet) {
    set.attached.retain(|a| a.weak.upgrade().is_some());
}

/// Attach an asker to a session. Returns the token that detaches it.
pub fn attach_session_asker(session_id: &str, asker: Arc<dyn Asker>) -> AskerId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = AskerId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let mut map = session_askers_map().lock_ok();
    let set = map
        .entry(session_id.to_string())
        .or_insert_with(|| AskerSet {
            attached: Vec::new(),
            shim_pins: Vec::new(),
        });
    prune(set);
    set.attached.push(AttachedAsker {
        id,
        weak: Arc::downgrade(&asker),
        detached: Arc::new(tokio::sync::Notify::new()),
    });
    id
}

/// Detach one asker by its token (the surface went away). Unknown or
/// already-detached tokens are a no-op — one surface's cleanup can never
/// erase another's route to the human. Detaching FIRES the attachment's
/// cancel handle, which is what ends a relay task still parked in this
/// surface's `ask()` (see [`AttachedAsker::detached`]).
pub fn detach_session_asker(session_id: &str, id: AskerId) {
    if id.0 == 0 {
        return;
    }
    let mut map = session_askers_map().lock_ok();
    if let Some(set) = map.get_mut(session_id) {
        let mut fire = None;
        set.attached.retain(|a| {
            if a.id == id {
                fire = Some(a.detached.clone());
                false
            } else {
                true
            }
        });
        set.shim_pins.retain(|(i, _)| *i != id);
        if let Some(n) = fire {
            n.notify_one();
        }
    }
}

/// The asker this session's next question registers through: the most
/// recently ATTACHED live one (the surface that took over the session
/// most recently owns its asks' bindings).
pub fn session_asker(session_id: &str) -> Option<Arc<dyn Asker>> {
    let mut map = session_askers_map().lock_ok();
    let set = map.get_mut(session_id)?;
    prune(set);
    set.attached.last().and_then(|a| a.weak.upgrade())
}

/// Every live attachment on the session, most recent first — the legs a
/// new ask is relayed to, each with the token and cancel handle needed
/// to reach it. Internal: the bystander relay in [`ask_session`] is the
/// one caller that needs the handles, not just the askers.
fn session_attachments(
    session_id: &str,
) -> Vec<(AskerId, Arc<dyn Asker>, Arc<tokio::sync::Notify>)> {
    let mut map = session_askers_map().lock_ok();
    let Some(set) = map.get_mut(session_id) else {
        return Vec::new();
    };
    prune(set);
    set.attached
        .iter()
        .rev()
        .filter_map(|a| Some((a.id, a.weak.upgrade()?, a.detached.clone())))
        .collect()
}

/// Every live asker on the session, most recent first — the legs a new
/// ask is relayed to.
pub fn session_askers(session_id: &str) -> Vec<Arc<dyn Asker>> {
    session_attachments(session_id)
        .into_iter()
        .map(|(_, a, _)| a)
        .collect()
}

// ── the ask that follows you off the surface it was born on ──────────
//
// PENDING §"Cross-session clarify relay", USER verbatim: "in that case
// I should be asked on telegram? why is nothing asking the clarify at
// all?" — a `clarify` from a desktop session timed out unanswered while
// the owner was on Telegram the whole time.
//
// `ask_session`'s fan-out is per SESSION ID: every live leg of the same
// session gets the question. A desktop session and the owner's Telegram
// DM are different session ids, so a relay that is exactly right as
// designed reaches nobody who has walked away from the machine.
//
// This is the cross-session half. One fallback leg, registered by the
// gateway at boot when `[notify] ask_relay` and a home channel are
// configured, joined LATE — after a grace delay, and only if the
// question is still open. Everything after that is machinery that
// already exists: it joins the same ask id through `register`, so the
// first answer anywhere resolves every leg and `ask-resolved` closes
// the dialogs on the surfaces that lost.
struct OwnerFallback {
    asker: Arc<dyn Asker>,
    delay_secs: u64,
    kinds: Vec<String>,
    /// `platform:chat` this leg lands in, so an ask BORN there is not
    /// relayed to itself — the owner would be asked the same question
    /// twice in one window.
    chat_key: String,
}

static OWNER_FALLBACK: OnceLock<crate::owned::OwnedSlot<OwnerFallback>> = OnceLock::new();

fn owner_fallback() -> &'static crate::owned::OwnedSlot<OwnerFallback> {
    OWNER_FALLBACK.get_or_init(crate::owned::OwnedSlot::default)
}

/// Install the owner's off-surface leg. Called once, by the gateway,
/// when `[notify] ask_relay = true` and a home channel is configured.
/// The owner is read from the calling thread (M4).
pub fn set_owner_fallback(
    asker: Arc<dyn Asker>,
    chat_key: &str,
    delay_secs: u64,
    kinds: Vec<String>,
) {
    owner_fallback().set(OwnerFallback {
        asker,
        delay_secs,
        kinds,
        chat_key: chat_key.to_string(),
    });
}

/// Forget it (test teardown, and a config reload that turns it off).
pub fn clear_owner_fallback() {
    owner_fallback().clear();
}

/// Uninstall the fallback ONLY if `owner` installed it (R3 teardown).
/// `None` (core) never matches. Returns whether it cleared.
pub fn purge_owner_fallback(owner: Option<&'static str>) -> bool {
    owner_fallback().purge(owner)
}

/// The owner's off-surface leg as installed, for a caller that asks the
/// OWNER directly rather than as a late leg of a session's own ask: the
/// sync decisions held for the owner (`syncask`). No delay and no kinds
/// filter apply — those rules are about following a session's question
/// off its surface, and these questions have no surface of their own.
/// `None`: `[notify] ask_relay` is off, or no home chat is configured.
pub fn owner_leg() -> Option<Arc<dyn Asker>> {
    owner_fallback().with(|fb| fb.asker.clone())
}

/// Is this ask still waiting for somebody?
pub fn is_open(id: &str) -> bool {
    pending().lock_ok().contains_key(id)
}

fn kind_word(k: AskKind) -> &'static str {
    match k {
        AskKind::Approval => "approval",
        AskKind::Clarify => "clarify",
        AskKind::Failover => "failover",
        AskKind::Secret => "secret",
        AskKind::Ticket => "ticket",
        AskKind::Challenge => "challenge",
    }
}

/// May this session's questions follow the owner off-surface?
///
/// A guest's approval must never be routed to the owner's phone: it is
/// the owner's attention being spent, and worse, the owner would be
/// answering a question about somebody else's turn without the context
/// to judge it. A session with NO registered principal is an owner
/// surface by construction — `with_principal` is what a non-owner
/// session gets, and the comment on it says owner surfaces never call
/// it.
pub fn owner_session(session_id: &str) -> bool {
    match crate::acl::session_principal(session_id) {
        None => true,
        Some(p) => p.role == crate::acl::Role::Owner,
    }
}

/// Spawn the delayed off-surface leg for one ask, if it is wanted.
///
/// Returns the destination it will reach, for the caller to SAY — an
/// answer that arrives from somewhere else must not look like the
/// surface you are sitting at produced it, which is the rule
/// `ask-resolved` already follows.
fn spawn_owner_fallback(req: &AskRequest, head_chat: Option<&str>) -> Option<String> {
    // A secret is never asked for on the owner's phone, whatever the
    // fallback's kinds list says.
    if req.kind == AskKind::Secret {
        return None;
    }
    let (asker, delay, chat_key) = owner_fallback().with(|fb| {
        if !fb.kinds.iter().any(|k| k == kind_word(req.kind)) {
            return None;
        }
        // Born in that very chat: the owner is already looking at it.
        if head_chat == Some(fb.chat_key.as_str()) {
            return None;
        }
        Some((fb.asker.clone(), fb.delay_secs, fb.chat_key.clone()))
    })??;
    if !owner_session(&req.session_id) {
        return None;
    }
    let req = req.clone();
    let id = req.id.clone();
    let where_to = chat_key.clone();
    crate::rt::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        // STILL OPEN? Answering at the keyboard inside the grace window
        // must leave the phone quiet — that is the whole reason for the
        // delay. `register` would refuse a tombstoned id anyway, but
        // checking here means the adapter is never even asked to render
        // a question nobody is waiting for.
        if !is_open(&id) {
            return;
        }
        // BORN THERE, read at fire time: the head leg registered with
        // its chat by now, whoever spawned this leg knew it or not (an
        // approval asked in the owner's own DM).
        let born = pending()
            .lock_ok()
            .get(&id)
            .and_then(|p| p.chat_key.clone());
        if born.as_deref() == Some(where_to.as_str()) {
            return;
        }
        tracing::info!("ask {id}: unanswered after {delay}s, relaying to the owner");
        // SAY IT ON THE SURFACE THAT ASKED. A question quietly forwarded
        // to somebody's phone is exactly the silent automation this
        // codebase refuses: the human sitting at the desktop has to know
        // their pocket just buzzed, and know why, or a later "answered
        // elsewhere" is the first they hear of any of it.
        crate::session_tap::publish(
            &req.session_id,
            "ask-relayed",
            json!({
                "id": id,
                "to": where_to,
                "after_secs": delay,
            }),
        );
        let _ = asker.ask(req).await;
    });
    Some(chat_key)
}

/// The owner's off-surface leg for an ask that is NOT asked through
/// [`ask_session`] — approvals and failover questions go straight to the
/// session's own asker (agent.rs). The same rules hold: a kind the
/// config lists, an owner's session, after the delay, only while still
/// open, never back into the chat it was born in. Returns where it will
/// go, if anywhere.
pub fn relay_to_owner(req: &AskRequest) -> Option<String> {
    spawn_owner_fallback(req, None)
}

/// Ask one session's humans, relaying when more than one surface is
/// attached: every INTERACTIVE leg renders the question its own way
/// and parks under the SAME ask id; the first answer resolves the ask
/// for every leg (see `register`/`resolve`), so dialogs close on all
/// surfaces without a coordinator. The HEAD leg's answer is what the
/// caller returns.
///
/// No legs (or none interactive) → `None`, which callers translate to
/// their honest "no human" answers (`clarify`'s `no_human()`, plan
/// mode's stays-in-plan).
pub async fn ask_session(session_id: &str, req: AskRequest) -> Option<AskAnswer> {
    let secret = req.kind == AskKind::Secret;
    // A turn whose questions belong to someone else ([`set_turn_asker`]:
    // the owner's line in a session they joined) asks THEM, and nobody
    // the session would otherwise ask.
    if let Some(a) = turn_asker(session_id) {
        if !a.interactive() || (secret && !a.private_input()) {
            return None;
        }
        spawn_owner_fallback(&req, None);
        return Some(a.ask(req).await);
    }
    let legs: Vec<(AskerId, Arc<dyn Asker>, Arc<tokio::sync::Notify>)> =
        session_attachments(session_id)
            .into_iter()
            .filter(|(_, a, _)| a.interactive())
            .filter(|(_, a, _)| !secret || a.private_input())
            .collect();
    let mut it = legs.into_iter();
    let (_head_id, head, _head_notify) = it.next()?;
    // Bystander relay is OPT-IN per leg (`wants_relay`): a shared IRC
    // channel has other people in it, and a desktop-born approval
    // dumped there is both a leak and a way for a bystander to think
    // they should answer (USER 2026-09-14). Single-user surfaces
    // (telegram, desktop, tui) keep the default true. The head is
    // exempt by construction — it was chosen, not relayed to.
    for (_id, bystander, detached) in it.filter(|(_, a, _)| a.wants_relay()) {
        let req = req.clone();
        // The strong Arc is held for ONE ask().await, scoped by the
        // select below: a surface that DETACHES while the ask is parked
        // ends this task through the cancel handle (its `ask` future is
        // dropped mid-poll — the drop-self path, never `cancel`, which
        // would end the ask for every surface). Before the handle
        // existed, the task held the Arc for the whole await no matter
        // what the registry said, and a detached surface stayed
        // "attached" until the question resolved — re-relayed to on the
        // next ask, its dead adapter asked to render questions nobody
        // would see (the bounded zombie). PENDING 2026-09-15 audit
        // round, closed 2026-09-19.
        crate::rt::spawn(async move {
            tokio::select! {
                biased;
                _ = detached.notified() => {
                    // The surface went away. The leg it registered (if
                    // the ask() got that far) is dropped by the AskLeg
                    // handle's own future being cancelled here; the
                    // ask itself stays open for every other leg.
                }
                a = bystander.ask(req) => {
                    let _ = a;
                }
            }
        });
    }
    // And the owner's off-surface leg, on a timer. Deliberately AFTER
    // the same-session fan-out and independent of it: those legs are
    // other views of this session, this one is another place the same
    // human might be.
    spawn_owner_fallback(&req, None);
    Some(head.ask(req).await)
}

// ── a turn whose questions go to someone else ────────────────────────
//
// The owner's own line in a session they joined (`crate::join`) runs
// with the owner's rights. Its approvals and questions are therefore the
// owner's to answer, in the owner's chat: the session's own askers reach
// the person the session belongs to, who must not be able to approve
// what runs with the owner's rights. Set for the length of that one turn
// (`Agent::run_turn`) and read before the session's askers by
// [`ask_session`] and by `Agent::asker` (approvals and failover ask
// through the agent's own asker, not through `ask_session`).

fn turn_askers() -> &'static Mutex<HashMap<String, Arc<dyn Asker>>> {
    static TURN_ASKERS: OnceLock<Mutex<HashMap<String, Arc<dyn Asker>>>> = OnceLock::new();
    TURN_ASKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Send `session_id`'s questions to `asker` until [`clear_turn_asker`].
pub fn set_turn_asker(session_id: &str, asker: Arc<dyn Asker>) {
    turn_askers()
        .lock_ok()
        .insert(session_id.to_string(), asker);
}

/// The turn that [`set_turn_asker`] was for ended.
pub fn clear_turn_asker(session_id: &str) {
    turn_askers().lock_ok().remove(session_id);
}

/// Where `session_id`'s questions go instead of its own askers, while a
/// turn says so.
pub fn turn_asker(session_id: &str) -> Option<Arc<dyn Asker>> {
    turn_askers().lock_ok().get(session_id).cloned()
}

/// A bot challenge to solve (PLAN-challenge-relay.md, D2). The owner's
/// own sessions ask as any question does. Anyone else's ask BOTH the
/// person whose session it is (privately: a challenge in a channel goes
/// to the asker's query, see [`ChatAsker`]) AND the owner, at once,
/// with no fallback delay and whatever `ask_relay_kinds` lists: the
/// owner asked to be the one who can always solve them. One ask id, so
/// the first answer is everyone's.
///
/// A leg that cannot ask (no route, no adapter) ends with a timeout;
/// that ends only that leg. The answer is the first one a person gave,
/// or a timeout once every leg has ended without one.
pub async fn ask_challenge(session_id: &str, req: AskRequest) -> Option<AskAnswer> {
    if owner_session(session_id) {
        return ask_session(session_id, req).await;
    }
    let owner = owner_leg().map(|leg| {
        crate::session_tap::publish(
            &req.session_id,
            "ask-relayed",
            json!({ "id": req.id, "to": "owner", "after_secs": 0 }),
        );
        let r = req.clone();
        async move { Some(leg.ask(r).await) }
    });
    let mine = ask_session(session_id, req);
    let theirs = async move {
        match owner {
            Some(f) => f.await,
            None => None,
        }
    };
    tokio::pin!(mine, theirs);
    let (mut mine_done, mut theirs_done) = (false, false);
    let mut last: Option<AskAnswer> = None;
    loop {
        let got = tokio::select! {
            a = &mut mine, if !mine_done => { mine_done = true; a }
            a = &mut theirs, if !theirs_done => { theirs_done = true; a }
            else => return last,
        };
        match got {
            Some(a) if !a.timed_out => return Some(a),
            Some(a) => last = Some(a),
            None => {}
        }
    }
}

/// Back-compat shim: the old single-slot spelling, used by tests that
/// do not care about tokens.
pub fn set_session_asker(session_id: &str, asker: Arc<dyn Asker>) {
    let id = attach_session_asker(session_id, asker.clone());
    // PIN: this spelling has no owner holding the Arc. Without a pin
    // the Weak died with the install's temporary and every shim-using
    // fixture stopped receiving questions (see `AskerSet::shim_pins`).
    let mut map = session_askers_map().lock_ok();
    if let Some(set) = map.get_mut(session_id) {
        set.shim_pins.push((id, asker));
    }
}

/// Back-compat shim for the old remove-by-id. The bug this file's
/// header describes lived here: it erased whatever asker ANY lifecycle
/// had installed. Detach is by token now; the only legitimate user of
/// this spelling is a test tearing down its fixture.
pub fn clear_session_asker(session_id: &str) {
    session_askers_map().lock_ok().remove(session_id);
}

/// Every open ask, oldest first — lets a frontend that reconnected
/// (desktop reload) redraw questions it missed.
pub fn open_all() -> Vec<AskRequest> {
    let map = pending().lock_ok();
    let mut v: Vec<(u64, AskRequest)> = map.values().map(|p| (p.seq, p.req.clone())).collect();
    v.sort_by_key(|(s, _)| *s);
    v.into_iter().map(|(_, r)| r).collect()
}

// ── pure helpers (unit-tested) ───────────────────────────────────────

/// Interpret a free-text reply against an ask. `None` = not an answer,
/// so the adapter should treat the message as ordinary input.
///
/// Accepted: a 1-based choice number, a choice key, a choice label
/// prefix, and the obvious yes/no colloquialisms for approvals. Clarify
/// asks with no choices accept any non-empty text.
pub fn parse_reply(req: &AskRequest, text: &str) -> Option<AskAnswer> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();

    // Multi-question ask (clarify parity): the whole reply answers
    // every question in one message. Accepted shapes, most structured
    // first:
    //   * a JSON object keyed by question key, ordinal or prompt:
    //     {"q1":"1","q2":"free text"} / {"1":"yes","2":3} — values
    //     may be strings, numbers or arrays (several selections);
    //     questions the object does not name are answered as EMPTY
    //     (explicitly skipped, not silently dropped)
    //   * labelled lines, one per question: "1: 2" / "3: free text"
    //     — the typed form for a phone or IRC, and the only practical
    //     one for a free-text question; unnamed questions are empty
    //   * positional: one answer per question, separated by newline,
    //     `;` or `|` — and by `,` only when no question is
    //     multi-select (a comma is a multi-select question's own
    //     separator). Every question must have choices and the count
    //     must match exactly
    // AMBIGUOUS INPUT IS REJECTED (USER, 2026-09-26: "if the input is
    // genuinely ambiguous then it's probably best to reject and have
    // the model retry with corrected shape. that's worth one
    // round-trip extra"). One free-text blob cannot be split across
    // several questions without guessing — and anything typed in the
    // channel would otherwise BE that guess (a bystander's chatter
    // answering every question with its text). A rejected reply is
    // not consumed: it flows on as an ordinary message and the ask
    // stays open for a properly-shaped one — the same convention
    // approvals have always had for prose.
    if req.questions.len() > 1 {
        return parse_multi_question_reply(req, t);
    }

    // A ticket ask is answered by the ticket itself, wherever it sits in
    // the message (a pasted line keeps whatever the chat client put
    // around it) — or declined.
    if req.kind == AskKind::Ticket {
        if crate::ticket::Ticket::parse(t).is_ok() {
            let start = t.find(crate::ticket::TICKET_PREFIX).unwrap_or(0);
            let token: String = t[start..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect();
            return Some(AskAnswer::text(&token));
        }
        if ["n", "no", "decline", "deny", "cancel"].contains(&lower.as_str()) {
            return Some(AskAnswer::choice(CHOICE_TICKET_DECLINE));
        }
        return None;
    }

    // A challenge is answered with what to do on the page, or a verdict
    // — by its word or by the number the prompt shows beside it (a "1"
    // that answered nothing would go to the model as a new message).
    if req.kind == AskKind::Challenge {
        if let Some(c) = t
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| req.choices.get(i))
        {
            return Some(AskAnswer::choice(&c.key));
        }
        return match lower.as_str() {
            "solved" | "done" | "ok" => Some(AskAnswer::choice(CHOICE_CHALLENGE_SOLVED)),
            "give up" | "give_up" | "giveup" | "no" | "cancel" | "stop" => {
                Some(AskAnswer::choice(CHOICE_CHALLENGE_GIVE_UP))
            }
            "window" | "open window" => Some(AskAnswer::choice(CHOICE_CHALLENGE_WINDOW)),
            _ => crate::challenge::Action::parse(t).map(|_| AskAnswer::text(t.trim())),
        };
    }

    // Multi-select clarify: "1,3" / "1 3".
    if req.multi && !req.choices.is_empty() {
        let parts: Vec<&str> = lower
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let mut picked = Vec::new();
        for p in &parts {
            if let Some(k) = choice_by_token(req, p) {
                picked.push(k);
            }
        }
        if picked.len() == parts.len() && !picked.is_empty() {
            return Some(AskAnswer {
                selected: picked,
                ..Default::default()
            });
        }
    }

    if let Some(k) = choice_by_token(req, &lower) {
        return Some(AskAnswer::choice(&k));
    }

    if req.kind == AskKind::Approval {
        // Colloquial yes/no. "yes" means allow ONCE — the broader
        // scopes must be asked for by name, never inferred.
        let yes = ["y", "yes", "ok", "okay", "sure", "allow", "approve", "go"];
        let no = [
            "n", "no", "nope", "deny", "reject", "stop", "cancel", "abort",
        ];
        // The contract every approval consumer reads (syncask's `act`,
        // each `PendingSource::decide`, `present_plan`): a plain yes is
        // `once` and a plain no is `deny`, WHATEVER the ask's own keys,
        // and the consumer maps them — "no" drops a held config change,
        // keeps a plan in planning. Do not translate them here.
        if yes.contains(&lower.as_str()) {
            return Some(AskAnswer::choice(CHOICE_ONCE));
        }
        if no.contains(&lower.as_str()) {
            return Some(AskAnswer::choice(CHOICE_DENY));
        }
        // S10: an amend reply ("amend: {json}" or "1 amend: {json}")
        // answers allow-once WITH the rewrite payload; the approval
        // gate applies it. Any other prose is not an answer.
        let rest = lower
            .strip_prefix("1 ")
            .or_else(|| lower.strip_prefix("once "))
            .unwrap_or(&lower);
        if rest.starts_with("amend:") {
            return Some(AskAnswer {
                selected: vec![CHOICE_ONCE.to_string()],
                free_text: t.trim().to_string().into(),
                timed_out: false,
                ..Default::default()
            });
        }
        // A choice and a note, the typed spelling of the desktop's note
        // box (USER 2026-10-10: notes were dropped everywhere): `2 drop
        // step 3`, `approve but skip the docs`, `no, use git clean`. The
        // FIRST word must name a choice on its own — a number, a key, a
        // label or a yes/no word — so prose that merely starts like one
        // ("nothing yet") still answers nothing.
        let trimmed = t.trim();
        if let Some((head, note)) = trimmed.split_once(char::is_whitespace) {
            let head_l = head
                .trim_end_matches([',', ':', ';', '.', '!'])
                .to_ascii_lowercase();
            let note = note.trim();
            // The ask's own key first, as the one-word path does: on a
            // plan "approve" IS a key; elsewhere it is a colloquial yes.
            let key = choice_by_exact_token(req, &head_l).or_else(|| {
                if yes.contains(&head_l.as_str()) {
                    Some(CHOICE_ONCE.to_string())
                } else if no.contains(&head_l.as_str()) {
                    Some(CHOICE_DENY.to_string())
                } else {
                    None
                }
            });
            if let (Some(key), false) = (key, note.is_empty()) {
                return Some(AskAnswer {
                    selected: vec![key],
                    free_text: Some(note.to_string()),
                    ..Default::default()
                });
            }
        }
        return None;
    }

    // Clarify: anything else is a free-text answer.
    Some(AskAnswer::text(t))
}

/// [`choice_by_token`] without the label-prefix match: a number, a key
/// or a whole label. Used where the token is the first word of a longer
/// reply, and "nothing yet" must not read as the choice "no…".
fn choice_by_exact_token(req: &AskRequest, token: &str) -> Option<String> {
    if let Ok(n) = token.parse::<usize>() {
        if n >= 1 && n <= req.choices.len() {
            return Some(req.choices[n - 1].key.clone());
        }
    }
    req.choices
        .iter()
        .find(|c| c.key.eq_ignore_ascii_case(token) || c.label.eq_ignore_ascii_case(token))
        .map(|c| c.key.clone())
}

fn choice_by_token(req: &AskRequest, token: &str) -> Option<String> {
    if let Ok(n) = token.parse::<usize>() {
        if n >= 1 && n <= req.choices.len() {
            return Some(req.choices[n - 1].key.clone());
        }
    }
    req.choices
        .iter()
        .find(|c| {
            c.key.eq_ignore_ascii_case(token)
                || c.label.eq_ignore_ascii_case(token)
                || (token.len() >= 3 && c.label.to_ascii_lowercase().starts_with(token))
        })
        .map(|c| c.key.clone())
}

/// A structured multi answer from a driver: either a list of choice
/// keys (one multi-select question) or an object keyed by question
/// key with `selected`/`text`. Shared by the CLI's and desktop's
/// decide paths so the two frontends cannot drift apart.
pub fn answers_value(v: &serde_json::Value) -> Option<AskAnswer> {
    match v {
        serde_json::Value::Array(items) => {
            let mut selected = Vec::new();
            for it in items {
                let s = it.as_str()?.trim().to_string();
                if s.is_empty() {
                    return None;
                }
                selected.push(s);
            }
            if selected.is_empty() {
                return None;
            }
            Some(AskAnswer {
                selected,
                ..Default::default()
            })
        }
        serde_json::Value::Object(map) => {
            let mut per = std::collections::BTreeMap::new();
            for (qk, val) in map {
                let obj = val.as_object()?;
                let mut a = AskAnswer::default();
                if let Some(sel) = obj.get("selected").and_then(|s| s.as_array()) {
                    for k in sel {
                        a.selected.push(k.as_str()?.to_string());
                    }
                }
                if let Some(t) = obj.get("text").and_then(|t| t.as_str()) {
                    a.free_text = Some(t.to_string());
                }
                if a.selected.is_empty() && a.free_text.as_deref().unwrap_or("").trim().is_empty() {
                    return None;
                }
                per.insert(qk.clone(), a);
            }
            if per.is_empty() {
                return None;
            }
            Some(AskAnswer::from_per_question(per))
        }
        _ => None,
    }
}

// ── multi-question replies ───────────────────────────────────────────
//
// The claude-code parity shape: several questions in one ask, one
// reply message answering all of them. Everything below exists so the
// TYPED path (IRC, telegram-as-text, CLI bare words) matches what the
// widget surfaces (TUI cursor, Tk dialog, telegram toggle buttons)
// can express, and the least-structured thing a human plausibly types
// works.

/// The single-question answer for `text` with `choices`/`multi` —
/// the shared per-question half of both reply parsers. Same grammar
/// as `parse_reply`'s single-question arms (multi-select "1,3",
/// one token, free text), minus the approval colloquialisms (an
/// approval is never a multi-question ask) and minus empty input.
fn question_answer(prompt: &str, choices: &[AskChoice], multi: bool, text: &str) -> AskAnswer {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    let mut req = AskRequest::new("", AskKind::Clarify, prompt, choices.to_vec());
    req.multi = multi;
    if multi && !choices.is_empty() {
        let parts: Vec<&str> = lower
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let mut picked = Vec::new();
        for p in &parts {
            if let Some(k) = choice_by_token(&req, p) {
                picked.push(k);
            }
        }
        if picked.len() == parts.len() && !picked.is_empty() {
            return AskAnswer {
                selected: picked,
                ..Default::default()
            };
        }
    }
    if let Some(k) = choice_by_token(&req, &lower) {
        return AskAnswer::choice(&k);
    }
    AskAnswer::text(t)
}

/// Parse one reply message against every question of a multi-question
/// ask. See `parse_reply`'s comment for the accepted shapes; `None`
/// = the reply does not name its questions unambiguously (rejected,
/// not guessed — the caller leaves the message alone and the ask
/// stays open).
fn parse_multi_question_reply(req: &AskRequest, t: &str) -> Option<AskAnswer> {
    use std::collections::BTreeMap;

    let questions = req.all_questions();

    // Shape 1: JSON object keyed by question key / ordinal / prompt.
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(t) {
        let mut answers: BTreeMap<String, AskAnswer> = BTreeMap::new();
        for (i, q) in questions.iter().enumerate() {
            let ordinal = (i + 1).to_string();
            let v = map
                .get(&q.key)
                .or_else(|| map.get(&ordinal))
                .or_else(|| map.get(&q.prompt));
            let Some(v) = v else { continue };
            let a = value_to_answer(q, v);
            answers.insert(q.key.clone(), a);
        }
        if !answers.is_empty() {
            // Every question appears in the answer — an unanswered one
            // as "(empty answer)" — so the model SEES which questions
            // the human skipped instead of wondering where they went.
            for q in &questions {
                answers.entry(q.key.clone()).or_default();
            }
            return Some(AskAnswer::from_per_question(answers));
        }
    }

    // Shape 2: labelled lines — EVERY non-empty line is `N: answer`
    // (or `qN: answer`) naming a distinct question. One unlabelled line
    // and it is not this shape.
    let lines: Vec<&str> = t.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut labelled: BTreeMap<String, AskAnswer> = BTreeMap::new();
    let mut all_labelled = !lines.is_empty();
    for line in &lines {
        let Some((label, rest)) = line.split_once(':') else {
            all_labelled = false;
            break;
        };
        let label = label.trim().to_ascii_lowercase();
        let n = label
            .strip_prefix('q')
            .unwrap_or(&label)
            .parse::<usize>()
            .ok();
        let q = n
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| questions.get(i));
        let (Some(q), false) = (q, rest.trim().is_empty()) else {
            all_labelled = false;
            break;
        };
        if labelled.contains_key(&q.key) {
            all_labelled = false;
            break;
        }
        labelled.insert(
            q.key.clone(),
            question_answer(&q.prompt, &q.choices, q.multi, rest.trim()),
        );
    }
    if all_labelled && !labelled.is_empty() {
        for q in &questions {
            labelled.entry(q.key.clone()).or_default();
        }
        return Some(AskAnswer::from_per_question(labelled));
    }

    // Shape 3: positional — every question has choices, the parts
    // count matches, every part names a choice. A comma splits only
    // when no question is multi-select: "1,3" is ONE multi-select
    // answer, and reading it as q1=1, q2=3 is the silent guess this
    // parser refuses. Never `/`: labels carry it ("TCP/IP").
    if questions.iter().all(|q| !q.choices.is_empty()) {
        let any_multi = questions.iter().any(|q| q.multi);
        let parts: Vec<&str> = t
            .split(|c: char| matches!(c, '\n' | ';' | '|') || (c == ',' && !any_multi))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() == questions.len() {
            let mut answers: BTreeMap<String, AskAnswer> = BTreeMap::new();
            for (q, p) in questions.iter().zip(parts) {
                let a = question_answer(&q.prompt, &q.choices, q.multi, p);
                // A free-text fallthrough on a choices question means
                // the part did not name a choice — positional matching
                // must not turn that into a silently-wrong selection.
                if a.selected.is_empty() {
                    answers.clear();
                    break;
                }
                answers.insert(q.key.clone(), a);
            }
            if !answers.is_empty() {
                return Some(AskAnswer::from_per_question(answers));
            }
        }
    }

    // Ambiguous: rejected, not guessed (see the header comment).
    None
}

/// A JSON reply value → one question's answer. Strings parse with the
/// reply grammar, numbers are 1-based ordinals, arrays are several
/// selections (or several words for a free-text question).
fn value_to_answer(q: &AskQuestion, v: &serde_json::Value) -> AskAnswer {
    use serde_json::Value;
    match v {
        Value::String(s) => question_answer(&q.prompt, &q.choices, q.multi, s),
        Value::Number(n) => question_answer(&q.prompt, &q.choices, q.multi, &n.to_string()),
        Value::Array(items) => {
            let joined = items
                .iter()
                .map(|it| match it {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(",");
            // Join with commas so a multi-select question reads the
            // array as "1,3"; a single-select question whose array
            // holds several items falls to free text below.
            let a = question_answer(&q.prompt, &q.choices, q.multi, &joined);
            if a.selected.len() > 1 && !q.multi {
                // Several selections for a single-select question:
                // keep them (the human chose them) as the free-text
                // rendering rather than inventing a selection.
                return AskAnswer::text(&joined.replace(',', ", "));
            }
            a
        }
        other => AskAnswer::text(&other.to_string()),
    }
}

/// Text rendering for frontends without widgets (IRC, plain CLI).
pub fn render_prompt(req: &AskRequest) -> String {
    let mut s = String::new();
    let head = match req.kind {
        AskKind::Approval => "approval needed",
        AskKind::Clarify => "question",
        AskKind::Failover => "failover",
        AskKind::Secret => "secret needed",
        AskKind::Ticket => "ticket needed",
        AskKind::Challenge => "challenge",
    };
    s.push_str(&format!("[{head}] {}", req.prompt));
    if let Some(d) = &req.detail {
        let d = d.trim();
        if !d.is_empty() {
            // BOUNDED (plan-mode round 2026-09-13): a plan-mode
            // approval carries the WHOLE plan as detail, and an IRC
            // channel is line-length-limited — a 200-line plan was one
            // giant message (or several, sliced by the wire). Head and
            // tail of the plan are shown, with the elision honest
            // about what is missing; the full text is in the Tasks
            // pane / TUI popup / plan file.
            const MAX_DETAIL_LINES: usize = 30;
            let lines: Vec<&str> = d.lines().collect();
            if lines.len() <= MAX_DETAIL_LINES {
                s.push_str(&format!("\n  {}", d.replace('\n', "\n  ")));
            } else {
                let head_n = MAX_DETAIL_LINES * 2 / 3;
                let tail_n = MAX_DETAIL_LINES - head_n;
                let skipped = lines.len() - head_n - tail_n;
                let mut block = lines[..head_n].join("\n");
                block.push_str(&format!(
                    "\n  [… {skipped} lines elided — the full plan is in the \
                     plan file and the desktop Tasks pane …]"
                ));
                block.push('\n');
                block.push_str(&lines[lines.len() - tail_n..].join("\n"));
                s.push_str(&format!("\n  {}", block.replace('\n', "\n  ")));
            }
        }
    }

    // A secret is never typed into this prompt: it would sit in the
    // scrollback and, on a relayed surface, in a chat log. Say where it
    // goes instead.
    if req.kind == AskKind::Secret {
        s.push_str(&format!(
            "\n  Store it privately: run `veles secret set {}` in a terminal on this \
             machine (it does not echo), then answer 1. Never type the value here.",
            req.prompt
        ));
    }

    // ── the questions ────────────────────────────────────────────────
    // The affordance is NAMED. A multi-select ask whose prompt never
    // says "pick any" reads as single-answer: nobody knows to answer
    // "1,3" (the 2026-09-26 parity audit). Same for multi-question
    // asks — one message answers every question.
    if req.questions.len() > 1 {
        let n = req.questions.len();
        s.push_str(&format!("\n  ({n} questions — answer all in one reply)"));
        for (i, q) in req.all_questions().iter().enumerate() {
            s.push_str(&format!("\n  {}. {}", i + 1, q.prompt));
            for (j, c) in q.choices.iter().enumerate() {
                s.push_str(&format!("\n     {}) {}", j + 1, c.label));
            }
            if q.multi && !q.choices.is_empty() {
                s.push_str("\n     (pick any, e.g. \"1,3\")");
            }
            if q.choices.is_empty() {
                s.push_str("\n     (free text)");
            }
        }
        // The forms parse_reply accepts, in the words a human types. The
        // JSON object also parses (drivers send it), but nobody types
        // JSON on a phone, so it is not advertised here.
        let qs = req.all_questions();
        s.push_str(
            "\n  reply one line per question, e.g. \"1: 2\" — a number picks that \
choice, anything else is your own answer",
        );
        if qs.iter().all(|q| !q.choices.is_empty()) {
            let example = if qs.iter().any(|q| q.multi) {
                "1 3; 2"
            } else {
                "1, 2"
            };
            s.push_str(&format!(
                "\n  or every choice in order on one line, e.g. \"{example}\""
            ));
        }
    } else if !req.choices.is_empty() {
        let opts: Vec<String> = req
            .choices
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}) {}", i + 1, c.label))
            .collect();
        s.push_str(&format!("\n  {}", opts.join("  ")));
        if req.multi {
            s.push_str("\n  (pick any, e.g. \"1,3\")");
        }
    }
    s.push_str(&format!(
        "\n  reply within {}s (ask {})",
        req.timeout_secs, req.id
    ));
    s
}

/// Await an answer with the request's timeout. On timeout the ask is
/// dropped so a late reply can't resolve a dead turn.
pub async fn wait(req: &AskRequest, rx: tokio::sync::oneshot::Receiver<AskAnswer>) -> AskAnswer {
    match tokio::time::timeout(std::time::Duration::from_secs(req.timeout_secs), rx).await {
        Ok(Ok(a)) => a,
        Ok(Err(_)) => {
            // Sender dropped (cancelled) — treat as no answer.
            cancel(&req.id);
            AskAnswer::timeout()
        }
        Err(_) => {
            cancel(&req.id);
            tracing::info!("ask {} timed out after {}s", req.id, req.timeout_secs);
            AskAnswer::timeout()
        }
    }
}

// ── askers ───────────────────────────────────────────────────────────

/// No human is reachable (cron jobs, A2A peer sessions, `chat -q`,
/// delegated children). Answers from policy instead of asking.
pub struct PolicyAsker {
    /// What an approval ask resolves to.
    pub approve: bool,
}

#[async_trait]
impl Asker for PolicyAsker {
    async fn ask(&self, req: AskRequest) -> AskAnswer {
        match req.kind {
            AskKind::Approval => AskAnswer::choice(if self.approve {
                CHOICE_ONCE
            } else {
                CHOICE_DENY
            }),
            AskKind::Clarify => AskAnswer::text(
                "no interactive user is available in this session — proceed with your best judgment",
            ),
            // Non-interactive policy asker: never fall back silently on
            // its behalf — stop (the error surfaces to whoever runs the
            // headless job).
            AskKind::Failover => AskAnswer::choice("stop"),
            AskKind::Secret => AskAnswer::choice(CHOICE_SECRET_DECLINE),
            // Nobody here can sign: the call is refused, and says so.
            AskKind::Ticket => AskAnswer::choice(CHOICE_TICKET_DECLINE),
            // Nobody here can see the page, let alone solve it.
            AskKind::Challenge => AskAnswer::choice(CHOICE_CHALLENGE_GIVE_UP),
        }
    }
    fn interactive(&self) -> bool {
        false
    }
}

/// Hands the question to a frontend's own event loop over a plain
/// channel, then parks. Used by every surface that already has a loop
/// on another thread (CLI REPL, TUI) — the loop draws the prompt and
/// calls `resolve`.
pub struct ChannelAsker {
    pub tx: std::sync::mpsc::Sender<AskRequest>,
}

#[async_trait]
impl Asker for ChannelAsker {
    async fn ask(&self, req: AskRequest) -> AskAnswer {
        let Some(leg) = register(&req, None, None) else {
            return AskAnswer::timeout();
        };
        if self.tx.send(req.clone()).is_err() {
            leg.drop_self();
            tracing::warn!("ask {} undeliverable: frontend loop is gone", req.id);
            return AskAnswer::timeout();
        }
        leg.wait().await
    }
    /// The CLI REPL and the TUI: a terminal on the operator's machine.
    fn private_input(&self) -> bool {
        true
    }
}

/// Asks through a gateway chat (telegram, IRC): renders the question as
/// an outbound `Ask` message and parks on the pending registry until the
/// adapter resolves it.
pub struct ChatAsker {
    pub bus: crate::bus::GatewayBus,
    pub platform: String,
    pub chat_id: String,
    /// Verified identity of the sender whose session opens asks. Only
    /// they (or an owner) may answer them (S15 ACL).
    pub sender_id: Option<String>,
    /// True when this chat has an audience beyond the session's owner
    /// (an IRC channel with other people in it). Bystander asks born
    /// on other surfaces are NOT relayed here (USER 2026-09-14: "it
    /// would be weird to have everything advertised to irc though with
    /// other people in the channel"); this session's OWN questions
    /// still ask here — it is the head, and the head is never
    /// filtered. Telegram/desktop/tui are single-user: relay on.
    pub shared_audience: bool,
}

/// A private way to the person whose session this is, in a shared IRC
/// room: their nick (from the mask they spoke with) and the network it
/// was on, which names the bus route — with two networks a bare nick
/// could reach a stranger who holds it on the other one.
fn private_route(session_id: &str, platform: &str) -> Option<(String, String)> {
    if platform != "irc" {
        return None;
    }
    let ctx = crate::acl::session_sender(session_id)?;
    let nick = ctx.mask.as_deref()?.split('!').next()?.trim();
    let network = ctx.network.as_deref()?.trim();
    (!nick.is_empty() && !network.is_empty()).then(|| (nick.to_string(), network.to_string()))
}

#[async_trait]
impl Asker for ChatAsker {
    async fn ask(&self, req: AskRequest) -> AskAnswer {
        // A challenge carries a link that drives the agent's browser, so
        // in a shared room it goes to the asker alone, in a query on the
        // network the session came from (PLAN-challenge-relay.md, D2).
        // With no private way to them it is not posted at all: the
        // owner's leg still has it.
        let (chat_id, route) = if req.kind == AskKind::Challenge && self.shared_audience {
            match private_route(&req.session_id, &self.platform) {
                Some((nick, network)) => (nick, Some(network)),
                None => return AskAnswer::timeout(),
            }
        } else {
            (self.chat_id.clone(), None)
        };
        let chat_key = format!("{}:{}", self.platform, chat_id);
        let Some(leg) = register(&req, Some(&chat_key), self.sender_id.as_deref()) else {
            return AskAnswer::timeout();
        };
        let sent = self.bus.send_outbound_to(
            &self.platform,
            route.as_deref(),
            crate::bus::OutboundMessage {
                chat_id,
                reply_to: None,
                text: render_prompt(&req),
                reasoning: None,
                provider: String::new(),
                model: String::new(),
                kind: crate::bus::MessageKind::Ask,
                ask: Some(req.clone()),
                meta: Default::default(),
            },
        );
        if !sent {
            // No adapter listening — this LEG is dead, but on a relayed
            // ask the question stays open on every other surface.
            leg.drop_self();
            tracing::warn!("ask {} undeliverable: no {} adapter", req.id, self.platform);
            return AskAnswer::timeout();
        }
        leg.wait().await
    }

    /// An IRC channel with other people in it: no bystander relay
    /// (asks born elsewhere stay elsewhere). Telegram DMs and every
    /// single-user surface default to relay-on.
    fn wants_relay(&self) -> bool {
        !self.shared_audience
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_req() -> AskRequest {
        AskRequest::new(
            "s1",
            AskKind::Approval,
            "run `rm -rf /tmp/x`?",
            approval_choices(true),
        )
    }

    /// C3 — a live steer REPLACED the pending one. `Option::replace`
    /// returns the old value and this threw it away, so two redirects
    /// landing in the same gap between tool rounds — a correction typed
    /// right after the first, the ordinary case — silently lost one.
    /// That is the rule this module is built on ("a steer that cannot
    /// reach the current turn is a message that must not vanish"),
    /// broken on the very path written to protect it.
    ///
    /// Nothing covered the steer slot at all before this: neither the
    /// routing nor the replacement.
    #[test]
    fn a_second_live_steer_queues_behind_the_first_instead_of_replacing_it() {
        let sid = "steer-append-test";
        let handle: SteerHandle = Arc::new(Mutex::new(None));
        let _guard = claim_steer_slot(sid, handle.clone());

        assert_eq!(steer(sid, "use python"), Steered::Live);
        assert_eq!(handle.lock_ok().as_deref(), Some("use python"));

        // The correction, in the same gap.
        assert_eq!(
            steer(sid, "actually rust"),
            Steered::LiveQueued { behind: 1 },
            "the caller has to be able to SAY it queued behind another"
        );
        assert_eq!(
            handle.lock_ok().as_deref(),
            Some("use python\nactually rust"),
            "BOTH reach the turn — the first is not thrown away"
        );
        assert_eq!(
            steer(sid, "and add a test"),
            Steered::LiveQueued { behind: 2 }
        );

        // The turn drains the slot as one `[STEER]` message carrying
        // every redirect, which is how a human who typed them seconds
        // apart reads them back.
        let taken = handle.lock_ok().take().expect("the turn drains it");
        for want in ["use python", "actually rust", "and add a test"] {
            assert!(taken.contains(want), "{want} missing from {taken:?}");
        }
        // Drained → the next one is a fresh Live, not a queue-behind.
        assert_eq!(steer(sid, "one more"), Steered::Live);
    }

    /// The routing the slot exists for, also previously unpinned: with
    /// no live turn a steer PARKS rather than writing into a handle
    /// nobody reads, and the guard releases the slot when the turn ends
    /// so the next one parks again.
    #[test]
    fn a_steer_parks_when_no_turn_holds_the_slot() {
        let sid = "steer-park-test";
        assert_eq!(steer(sid, "nobody home"), Steered::Parked);
        {
            let handle: SteerHandle = Arc::new(Mutex::new(None));
            let _g = claim_steer_slot(sid, handle.clone());
            assert_eq!(steer(sid, "now someone is"), Steered::Live);
            assert_eq!(handle.lock_ok().as_deref(), Some("now someone is"));
        }
        assert_eq!(
            steer(sid, "gone again"),
            Steered::Parked,
            "the guard released the slot when the turn ended"
        );
        discard_docked_steers(sid);
    }

    /// Plan-mode approvals carry the WHOLE plan as detail; a chat
    /// surface renders it as plain text. The prompt must be BOUNDED:
    /// head and tail kept, elision named — never a 200-line IRC
    /// message, and never a silently-truncated one.
    #[test]
    fn a_plan_sized_detail_renders_bounded_with_named_elision() {
        let mut req = approval_req();
        let plan: String = (0..200)
            .map(|i| format!("## Step {i} — do the thing\n- a{i}\n- b{i}\n"))
            .collect();
        req.detail = Some(plan);
        let text = render_prompt(&req);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.len() <= 40,
            "bounded: head+tail+chrome, not 800 lines (got {})",
            lines.len()
        );
        assert!(
            text.contains("lines elided"),
            "the elision says what is missing: {text}"
        );
        assert!(text.contains("## Step 0"), "the head of the plan survives");
        assert!(
            text.contains("## Step 199"),
            "the tail of the plan survives (the last steps are what the reader is deciding on)"
        );
        // A SHORT detail is untouched — no elision chrome on a 5-liner.
        let mut small = approval_req();
        small.detail = Some("one\ntwo\nthree".into());
        let small_text = render_prompt(&small);
        assert!(small_text.contains("one"));
        assert!(!small_text.contains("elided"));
    }

    #[test]
    fn parses_numbers_keys_and_colloquialisms() {
        let r = approval_req();
        assert_eq!(parse_reply(&r, "1").unwrap().first(), Some(CHOICE_ONCE));
        assert_eq!(parse_reply(&r, "4").unwrap().first(), Some(CHOICE_DENY));
        assert_eq!(parse_reply(&r, "DENY").unwrap().first(), Some(CHOICE_DENY));
        assert_eq!(parse_reply(&r, " yes ").unwrap().first(), Some(CHOICE_ONCE));
        assert_eq!(parse_reply(&r, "no").unwrap().first(), Some(CHOICE_DENY));
        assert_eq!(
            parse_reply(&r, "always").unwrap().first(),
            Some(CHOICE_ALWAYS)
        );
        // Out of range and prose are NOT answers: the adapter must keep
        // treating those as ordinary chat.
        assert!(parse_reply(&r, "9").is_none());
        assert!(parse_reply(&r, "what does that command do?").is_none());
        assert!(parse_reply(&r, "").is_none());
    }

    /// A challenge is answered in words a chat user types: an action on
    /// the page, a verdict, or the number the prompt shows beside a
    /// choice. Anything else is not an answer, and flows on as chat.
    #[test]
    fn a_challenge_is_answered_in_words_or_by_its_numbers() {
        let r = AskRequest::new(
            "s",
            AskKind::Challenge,
            "cloudflare challenge on shop.example — solve it for the agent",
            vec![
                AskChoice::new(CHOICE_CHALLENGE_SOLVED, "solved"),
                AskChoice::new(CHOICE_CHALLENGE_GIVE_UP, "give up"),
                AskChoice::new(CHOICE_CHALLENGE_WINDOW, "open a window"),
            ],
        );
        let text = |t: &str| parse_reply(&r, t).and_then(|a| a.free_text);
        let pick = |t: &str| parse_reply(&r, t).and_then(|a| a.first().map(str::to_string));
        assert_eq!(text("click 200 140").as_deref(), Some("click 200 140"));
        assert_eq!(
            text(" type hello world ").as_deref(),
            Some("type hello world")
        );
        assert_eq!(text("key Enter").as_deref(), Some("key Enter"));
        assert_eq!(pick("Solved").as_deref(), Some(CHOICE_CHALLENGE_SOLVED));
        assert_eq!(pick("give up").as_deref(), Some(CHOICE_CHALLENGE_GIVE_UP));
        assert!(
            render_prompt(&r).contains("1) solved"),
            "the prompt numbers the choices"
        );
        assert_eq!(pick("1").as_deref(), Some(CHOICE_CHALLENGE_SOLVED));
        assert_eq!(pick("2").as_deref(), Some(CHOICE_CHALLENGE_GIVE_UP));
        assert_eq!(pick("3").as_deref(), Some(CHOICE_CHALLENGE_WINDOW));
        assert!(parse_reply(&r, "4").is_none());
        assert!(parse_reply(&r, "what is this page?").is_none());
        assert!(parse_reply(&r, "click here").is_none());
    }

    #[test]
    fn amend_reply_answers_once_with_text() {
        // S10: the gateway amend path — plain and prefixed with choice 1.
        let r = approval_req();
        for reply in [
            r#"amend: {"command": "echo safe"}"#,
            r#"1 amend: {"command": "echo safe"}"#,
        ] {
            let a = parse_reply(&r, reply).expect("amend must be an answer");
            assert_eq!(a.first(), Some(CHOICE_ONCE));
            assert_eq!(
                a.free_text.as_deref().map(str::trim),
                Some(reply.trim()),
                "free text preserved verbatim (the gate re-parses it)"
            );
        }
        // Prose containing 'amend' without the prefix is still not an answer.
        assert!(parse_reply(&r, "what does amend mean?").is_none());
    }

    /// The typed spelling of the note box (USER 2026-10-10): a choice
    /// first, then a note, on a guard approval and on a plan approval.
    #[test]
    fn a_choice_with_a_note_answers_both_and_prose_still_answers_nothing() {
        let r = approval_req();
        let a = parse_reply(&r, "1 keep the logs dir").expect("an answer");
        assert_eq!(a.first(), Some(CHOICE_ONCE));
        assert_eq!(a.free_text.as_deref(), Some("keep the logs dir"));
        let a = parse_reply(&r, "no, use git clean instead").expect("an answer");
        assert_eq!(a.first(), Some(CHOICE_DENY));
        assert_eq!(a.free_text.as_deref(), Some("use git clean instead"));
        for prose in ["nothing yet", "what does this do?", "allowance is fine"] {
            assert!(
                parse_reply(&r, prose).is_none(),
                "prose is not an answer: {prose}"
            );
        }

        // A plan approval: its keys are approve / keep-planning.
        let plan = AskRequest::new(
            "s1",
            AskKind::Approval,
            "Plan ready for approval",
            vec![
                AskChoice::new("approve", "approve and start work"),
                AskChoice::new("keep-planning", "keep planning"),
            ],
        );
        // A plain yes/no keeps the contract (`once`/`deny`); the plan
        // tool maps them (plan.rs), as syncask's `act` does for its own.
        assert_eq!(
            parse_reply(&plan, "yes").unwrap().first(),
            Some(CHOICE_ONCE)
        );
        assert_eq!(parse_reply(&plan, "no").unwrap().first(), Some(CHOICE_DENY));
        let a = parse_reply(&plan, "2 drop step 3").expect("an answer");
        assert_eq!(a.first(), Some("keep-planning"));
        assert_eq!(a.free_text.as_deref(), Some("drop step 3"));
        let a = parse_reply(&plan, "approve but skip the docs").expect("an answer");
        assert_eq!(a.first(), Some("approve"));
        assert_eq!(a.free_text.as_deref(), Some("but skip the docs"));
    }

    #[test]
    fn deny_override_hides_permanent_scopes() {
        let r = AskRequest::new(
            "s1",
            AskKind::Approval,
            "aux said deny",
            approval_choices(false),
        );
        assert_eq!(r.choices.len(), 2);
        assert_eq!(parse_reply(&r, "2").unwrap().first(), Some(CHOICE_DENY));
        // "always" is not offered, so it isn't accepted either.
        assert!(parse_reply(&r, "always").is_none());
    }

    #[test]
    fn clarify_takes_free_text_and_multi() {
        let mut r = AskRequest::new(
            "s1",
            AskKind::Clarify,
            "which?",
            vec![
                AskChoice::new("a", "apples"),
                AskChoice::new("b", "bananas"),
            ],
        );
        assert_eq!(parse_reply(&r, "2").unwrap().first(), Some("b"));
        assert_eq!(parse_reply(&r, "app").unwrap().first(), Some("a"));
        assert_eq!(
            parse_reply(&r, "neither, pears")
                .unwrap()
                .free_text
                .unwrap(),
            "neither, pears"
        );
        r.multi = true;
        let a = parse_reply(&r, "1,2").unwrap();
        assert_eq!(a.selected, vec!["a", "b"]);
    }

    // ── 2026-09-26 parity round: multi-question asks ─────────────────

    fn three_question_req() -> AskRequest {
        AskRequest::multi_question(
            "s1",
            AskKind::Clarify,
            vec![
                AskQuestion::new(
                    "q1",
                    "theme?",
                    vec![
                        AskChoice::new("c1", "synthwave"),
                        AskChoice::new("c2", "amber"),
                    ],
                ),
                AskQuestion::new(
                    "q2",
                    "art?",
                    vec![AskChoice::new("c1", "yes"), AskChoice::new("c2", "no")],
                )
                .with_multi(true),
                AskQuestion::new("q3", "name?", vec![]),
            ],
        )
    }

    /// The request's own prompt/choices mirror the first question so a
    /// consumer that predates the shape still shows something true.
    #[test]
    fn multi_question_request_mirrors_the_first_question() {
        let r = three_question_req();
        assert_eq!(r.prompt, "theme?");
        assert_eq!(r.choices.len(), 2);
        assert!(!r.multi);
        assert_eq!(r.questions.len(), 3);
        assert_eq!(r.all_questions().len(), 3);
        // Keys normalized when absent.
        assert_eq!(r.questions[0].key, "q1");
        // The wire keeps the field OFF for single-question asks
        // (back-compat with every existing consumer).
        let single = approval_req();
        let v = serde_json::to_value(&single).unwrap();
        assert!(v.get("questions").is_none());
    }

    /// Shape 1: a JSON reply keyed by question key (or ordinal).
    #[test]
    fn multi_question_json_reply_answers_every_question() {
        let r = three_question_req();
        let a = parse_reply(&r, r#"{"q1":"1","q2":"1,2","q3":"Imperial Grid"}"#).unwrap();
        assert_eq!(a.per_question.len(), 3, "{a:?}");
        assert_eq!(a.per_question["q1"].selected, vec!["c1"]);
        assert_eq!(a.per_question["q2"].selected, vec!["c1", "c2"]);
        assert_eq!(
            a.per_question["q3"].free_text.as_deref(),
            Some("Imperial Grid")
        );
        // Top-level mirrors the FIRST question (q1..q4 sort first).
        assert_eq!(a.first(), Some("c1"));
        // describe names each question.
        assert!(a.describe().contains("q1: c1"), "{}", a.describe());
        assert!(a.describe().contains("Imperial Grid"));
    }

    /// Shape 2: positional — every question choice-bound, separator-
    /// split, count-matched. A non-matching part rejects the whole
    /// attempt (never a partial pick).
    #[test]
    fn multi_question_positional_reply_matches_when_exact() {
        let r = AskRequest::multi_question(
            "s1",
            AskKind::Clarify,
            vec![
                AskQuestion::new(
                    "q1",
                    "first?",
                    vec![AskChoice::new("c1", "one"), AskChoice::new("c2", "two")],
                ),
                AskQuestion::new("q2", "second?", vec![AskChoice::new("c1", "alpha")]),
            ],
        );
        let a = parse_reply(&r, "2, 1").unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c2"]);
        assert_eq!(a.per_question["q2"].selected, vec!["c1"]);
        assert!(
            parse_reply(&r, "bananas, 1").is_none(),
            "a non-matching part rejects the whole positional attempt"
        );
    }

    /// Ambiguity REJECTS (USER, 2026-09-26: "if the input is genuinely
    /// ambiguous then it's probably best to reject and have the model
    /// retry with corrected shape. that's worth one round-trip
    /// extra"). Prose that does not name its questions is NOT an
    /// answer: None, the message flows on as chat, the ask stays
    /// open — a bystander's stray message must never decide a
    /// multi-question ask.
    #[test]
    fn ambiguous_multi_question_replies_are_rejected_not_guessed() {
        let r = three_question_req();
        assert!(
            parse_reply(&r, "do whatever looks best").is_none(),
            "prose is not an answer for a multi-question ask"
        );
    }

    /// A JSON object may answer SOME questions: the named ones carry
    /// their answers, the unnamed ones appear as explicit empties so
    /// the model sees the skip.
    #[test]
    fn a_partial_json_reply_names_its_questions_and_skips_the_rest() {
        let r = three_question_req();
        let a = parse_reply(&r, r#"{"q1":"1"}"#).unwrap();
        assert_eq!(a.per_question.len(), 3, "every question appears");
        assert_eq!(a.per_question["q1"].selected, vec!["c1"]);
        for skipped in ["q2", "q3"] {
            let sq = &a.per_question[skipped];
            assert!(
                sq.selected.is_empty() && sq.free_text.is_none(),
                "{skipped} is an explicit empty: {sq:?}"
            );
        }
    }

    /// A comma is a multi-select question's own separator ("pick any,
    /// e.g. 1,3"), so a positional reply may not ALSO split on it when
    /// any question is multi-select. "1,3" meant for q1 alone used to
    /// parse as q1=1, q2=3 — a silently wrong answer, the guess the
    /// rejection rule exists to refuse. Lines, `;` and `|` still split.
    #[test]
    fn a_comma_reply_is_not_split_across_questions_when_one_is_multi_select() {
        let r = AskRequest::multi_question(
            "s1",
            AskKind::Clarify,
            vec![
                AskQuestion::new(
                    "q1",
                    "toppings?",
                    vec![
                        AskChoice::new("c1", "olives"),
                        AskChoice::new("c2", "basil"),
                        AskChoice::new("c3", "chili"),
                    ],
                )
                .with_multi(true),
                AskQuestion::new(
                    "q2",
                    "size?",
                    vec![
                        AskChoice::new("c1", "small"),
                        AskChoice::new("c2", "medium"),
                        AskChoice::new("c3", "large"),
                    ],
                ),
            ],
        );
        assert!(
            parse_reply(&r, "1,3").is_none(),
            "ambiguous with a multi-select question: rejected"
        );
        let a = parse_reply(&r, "1,3; 2").unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c1", "c3"]);
        assert_eq!(a.per_question["q2"].selected, vec!["c2"]);
        let a = parse_reply(&r, "1 3\n2").unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c1", "c3"]);
    }

    /// A `/` is not a separator: labels carry them ("TCP/IP").
    #[test]
    fn a_slash_inside_a_label_is_not_a_separator() {
        let r = AskRequest::multi_question(
            "s1",
            AskKind::Clarify,
            vec![
                AskQuestion::new(
                    "q1",
                    "transport?",
                    vec![
                        AskChoice::new("c1", "TCP/IP"),
                        AskChoice::new("c2", "serial"),
                    ],
                ),
                AskQuestion::new("q2", "retry?", vec![AskChoice::new("c1", "yes")]),
            ],
        );
        let a = parse_reply(&r, "TCP/IP, 1").unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c1"]);
        assert_eq!(a.per_question["q2"].selected, vec!["c1"]);
    }

    /// The typed form a human can produce on a phone: one line per
    /// question, labelled with its number. It names its questions, so
    /// it is not a guess — and it is the only practical way to answer a
    /// FREE-TEXT question inside a multi-question ask on IRC or
    /// Telegram (positional needs every question to have choices; JSON
    /// is not something anyone types on a phone). Unnamed questions are
    /// explicit empties, like a partial JSON reply.
    #[test]
    fn labelled_lines_answer_their_questions() {
        let r = three_question_req();
        let a = parse_reply(&r, "1: 2\n3: Imperial Grid: the sequel").unwrap();
        assert_eq!(a.per_question.len(), 3, "{a:?}");
        assert_eq!(a.per_question["q1"].selected, vec!["c2"]);
        let q2 = &a.per_question["q2"];
        assert!(q2.selected.is_empty() && q2.free_text.is_none(), "{q2:?}");
        assert_eq!(
            a.per_question["q3"].free_text.as_deref(),
            Some("Imperial Grid: the sequel")
        );
        // A line that is not `N: …` makes it not this form.
        assert!(parse_reply(&r, "1: 2\nand also the name is X").is_none());
        // A number that names no question is not this form either.
        assert!(parse_reply(&r, "7: 2").is_none());
    }

    /// The prompt names the affordance (IRC/CLI): multi-select says
    /// "pick any", multi-question shows every question inline.
    #[test]
    fn render_prompt_names_the_affordance() {
        let mut r = AskRequest::new(
            "s1",
            AskKind::Clarify,
            "which?",
            vec![
                AskChoice::new("a", "apples"),
                AskChoice::new("b", "bananas"),
            ],
        );
        let single = render_prompt(&r);
        assert!(!single.contains("pick any"), "single-select stays lean");
        r.multi = true;
        let multi = render_prompt(&r);
        assert!(multi.contains("pick any"), "multi says so: {multi}");
        let mq = render_prompt(&three_question_req());
        assert!(mq.contains("3 questions"), "{mq}");
        assert!(mq.contains("2. art?"), "every question shown inline");
        assert!(!mq.contains("Imperial"), "choices not leaked");
    }

    /// The structured driver answer (CLI/desktop decide paths share it).
    #[test]
    fn answers_value_builds_multi_answers() {
        let list = serde_json::json!(["c1", "c3"]);
        let a = crate::ask::answers_value(&list).unwrap();
        assert_eq!(a.selected, vec!["c1", "c3"]);
        let obj = serde_json::json!({
            "q1": {"selected": ["c1"]},
            "q2": {"text": "Custom (a plan)"}
        });
        let a = crate::ask::answers_value(&obj).unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c1"]);
        assert_eq!(
            a.per_question["q2"].free_text.as_deref(),
            Some("Custom (a plan)")
        );
        // Junk is refused, not guessed.
        assert!(crate::ask::answers_value(&serde_json::json!("just a string")).is_none());
        assert!(crate::ask::answers_value(&serde_json::json!([])).is_none());
    }

    #[tokio::test]
    async fn resolve_for_chat_answers_the_oldest_ask() {
        // Bound to a SENDER, and answered by that sender. The binding
        // is then the whole answer and no ACL is consulted — this test
        // is about ordering, and reading the process-global ACL
        // snapshot for it made it race whichever neighbour had set a
        // curated one (`ask_answer_is_sender_bound` does).
        let chat = crate::testenv::chat_key();
        let r1 = approval_req();
        let r2 = approval_req();
        let who = Some("irc:alice");
        let rx1 = register(&r1, Some(&chat), who).expect("open ask");
        let _rx2 = register(&r2, Some(&chat), who);
        assert_eq!(open_for_chat(&chat).len(), 2);

        // A non-answer leaves both parked.
        assert!(resolve_for_chat(&chat, "hey what's that?", who).is_none());
        assert_eq!(open_for_chat(&chat).len(), 2);

        let id = resolve_for_chat(&chat, "1", who).unwrap();
        assert_eq!(id, r1.id);
        assert_eq!(rx1.wait().await.first(), Some(CHOICE_ONCE));
        assert_eq!(open_for_chat(&chat).len(), 1);
        cancel(&r2.id);
    }

    /// The owner override has to work for an owner recognised BY MASK —
    /// which on a services-less network is every owner there is.
    ///
    /// `may_answer` resolved the responder with the ctx-less
    /// `Principal::resolve`, so mask rules were invisible to it: an
    /// owner whose only identity is `me!me@host` was not an owner here.
    /// It failed closed, so nothing was exposed — but `/acl` said owner
    /// and the ask layer disagreed, and no test held either position.
    #[tokio::test]
    async fn a_mask_derived_owner_can_decide_someone_elses_ask() {
        use crate::acl::{Cap, Principal, Role};
        // The ACL snapshot this test writes is process-global, so it
        // takes the one lock (testenv rule 1): without it a neighbour
        // test's snapshot — or its clearing of this one — is what the
        // owner override below is checked against.
        let _g = crate::testenv::guard();
        set_acl_snapshot_for_tests(Some(
            serde_yaml::from_str("entries:\n  - identity: \"me!*@trusted.host\"\n    role: owner")
                .unwrap(),
        ));
        let chat = crate::testenv::chat_key();
        let r = AskRequest::new(
            "s-mask",
            AskKind::Approval,
            "run rm?",
            approval_choices(true),
        );
        let rx = register(&r, Some(&chat), Some("irc:net/alice!~a@elsewhere")).expect("open");

        // The mask-derived owner: no verified account, so `id` is None
        // and the SUBJECT is the wire mask — a string the ctx-less
        // resolver cannot match against a mask rule.
        let mut owner = Principal::guest();
        owner.subject = "irc:net/me!~me@trusted.host".into();
        owner.mask = Some("me!~me@trusted.host".into());
        owner.role = Role::Owner;
        owner.caps = Role::Owner.default_caps();
        assert!(owner.has(Cap::All), "the fixture must actually be an owner");

        // The old spelling — a bare subject — still cannot see it, which
        // is exactly the bug: nothing about this string says "owner".
        assert!(!may_answer(
            &r.id,
            Decider::sender(Some("irc:net/me!~me@trusted.host"))
        ));
        // Handed the resolved principal, the override fires.
        assert!(may_answer(&r.id, Decider::principal(&owner)));

        let id = resolve(&r.id, AskAnswer::choice(CHOICE_ONCE));
        assert!(id);
        assert_eq!(rx.wait().await.first(), Some(CHOICE_ONCE));

        // A mask-derived NON-owner is still refused: the override is
        // the owner's, not everybody's.
        let r2 = AskRequest::new("s-mask2", AskKind::Approval, "run?", approval_choices(true));
        let _rx2 = register(&r2, Some(&chat), Some("irc:net/alice!~a@elsewhere"));
        let mut nobody = Principal::guest();
        nobody.subject = "irc:net/bob!~b@nowhere".into();
        nobody.mask = Some("bob!~b@nowhere".into());
        assert!(!may_answer(&r2.id, Decider::principal(&nobody)));
        cancel(&r2.id);
        set_acl_snapshot_for_tests(None);
    }

    #[tokio::test]
    async fn ask_answer_is_sender_bound() {
        // The ACL snapshot this test writes is process-global, so it
        // takes the one lock (testenv rule 1): without it a neighbour
        // test's snapshot — or its clearing of this one — is what the
        // owner override below is checked against.
        let _g = crate::testenv::guard();
        // Curated ACL for this test (the ambient config may be legacy
        // mode where everyone is owner-like, which would defeat the
        // assertion).
        set_acl_snapshot_for_tests(Some(
            serde_yaml::from_str("owner: [\"irc:owner\"]").unwrap(),
        ));
        // S15: an ask opened by one sender cannot be answered by a
        // different sender in the same chat (a guest typing "1" must
        // not decide the owner's approval).
        let chat = crate::testenv::chat_key();
        let r = AskRequest::new("s1", AskKind::Approval, "run rm?", approval_choices(true));
        let rx = register(&r, Some(&chat), Some("irc:alice")).expect("open");
        assert_eq!(open_for_chat(&chat).len(), 1);

        // Bob (different sender, not owner) cannot answer — not by
        // number, not by colloquial yes.
        assert!(resolve_for_chat(&chat, "1", Some("irc:bob")).is_none());
        assert!(resolve_for_chat(&chat, "yes", Some("irc:bob")).is_none());
        assert_eq!(open_for_chat(&chat).len(), 1);

        // Alice (the bound sender) can.
        let id = resolve_for_chat(&chat, "1", Some("irc:alice")).unwrap();
        assert_eq!(id, r.id);
        assert_eq!(rx.wait().await.first(), Some(CHOICE_ONCE));

        // The owner outranks the binding (can answer others' asks).
        let r2 = AskRequest::new("s1b", AskKind::Approval, "run?", approval_choices(false));
        let rx2 = register(&r2, Some(&chat), Some("irc:alice")).expect("open");
        let id2 = resolve_for_chat(&chat, "deny", Some("irc:owner")).unwrap();
        assert_eq!(id2, r2.id);
        assert_eq!(rx2.wait().await.first(), Some(CHOICE_DENY));

        // An ask bound to a CHAT but to no sender — the platform could
        // not identify whoever opened it — is decided by the owner
        // alone. It used to be answerable by anybody, and on a
        // services-less network (masks, no NickServ) that was every
        // ask in every channel: a stranger typing "1" approved a tool
        // call. Restoring `None => true` for a chat-bound ask turns
        // this red.
        let r3 = AskRequest::new("s1c", AskKind::Approval, "run?", approval_choices(true));
        let rx3 = register(&r3, Some(&chat), None).expect("open");
        assert!(
            resolve_for_chat(&chat, "1", Some("irc:bob")).is_none(),
            "an unidentified ask must not be answerable by a stranger"
        );
        assert!(resolve_for_chat(&chat, "1", None).is_none());
        let id3 = resolve_for_chat(&chat, "1", Some("irc:owner")).expect("owner decides");
        assert_eq!(id3, r3.id);
        assert_eq!(rx3.wait().await.first(), Some(CHOICE_ONCE));

        set_acl_snapshot_for_tests(None);
    }

    #[tokio::test]
    async fn timeout_drops_the_ask() {
        let r = approval_req().with_timeout(5); // clamped minimum
        let rx = register(&r, None, None).expect("open ask");
        // Force the timeout path without waiting 5s.
        cancel(&r.id);
        drop(pending().lock().unwrap().remove(&r.id));
        let a = tokio::time::timeout(std::time::Duration::from_secs(1), rx.wait())
            .await
            .expect("wait returns as soon as the sender is dropped");
        assert!(a.timed_out);
        assert!(!super::pending().lock().unwrap().contains_key(&r.id));
    }

    #[tokio::test]
    async fn policy_asker_denies_and_answers_clarify() {
        let p = PolicyAsker { approve: false };
        let a = p.ask(approval_req()).await;
        assert_eq!(a.first(), Some(CHOICE_DENY));
        assert!(!p.interactive());
        let c = p
            .ask(AskRequest::new("s", AskKind::Clarify, "?", vec![]))
            .await;
        assert!(c.free_text.unwrap().contains("no interactive user"));
    }

    /// `resolve`/`cancel` publish `ask-resolved` on the session tap —
    /// that is how a surface whose human answered SECOND learns to
    /// close its dialog ("answered elsewhere"). Its future resolving
    /// is invisible to a surface that renders from the event stream.
    #[tokio::test]
    async fn resolving_publishes_ask_resolved_on_the_tap() {
        let mut rx = crate::session_tap::subscribe();
        let r = AskRequest::new("s-tap", AskKind::Approval, "run?", approval_choices(true))
            .with_timeout(30);
        let leg = register(&r, None, None).expect("open");
        assert!(resolve(&r.id, AskAnswer::choice(CHOICE_DENY)));
        // Broadcast is laggy-visible; poll briefly.
        let mut saw = false;
        for _ in 0..50 {
            match rx.try_recv() {
                Ok(ev) if ev.kind == "ask-resolved" && ev.session_id == "s-tap" => {
                    saw = true;
                    assert_eq!(ev.data["id"], json!(r.id), "the id names the ask");
                    assert_eq!(ev.data["by"], json!("answer"));
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(saw, "resolve published ask-resolved on the tap");
        // The leg got the answer too (deliver AND notify).
        assert_eq!(leg.wait().await.first(), Some(CHOICE_DENY));

        // cancel notifies as well (turn cancelled → dialogs close).
        let r2 = AskRequest::new("s-tap", AskKind::Approval, "again?", approval_choices(true))
            .with_timeout(30);
        let _leg2 = register(&r2, None, None).expect("open");
        cancel(&r2.id);
        let mut saw_cancel = false;
        for _ in 0..50 {
            match rx.try_recv() {
                Ok(ev) if ev.kind == "ask-resolved" && ev.session_id == "s-tap" => {
                    assert_eq!(ev.data["id"], json!(r2.id));
                    assert_eq!(ev.data["by"], json!("cancel"));
                    saw_cancel = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_cancel, "cancel published ask-resolved on the tap");
    }

    /// Records every ask it renders. Asker-shape of a shared IRC
    /// channel: interactive, but opts out of bystander relay.
    struct RecordingOptOut {
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Asker for RecordingOptOut {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            self.seen.lock().unwrap().push(req.prompt.clone());
            AskAnswer::text("from the opt-out leg")
        }
        fn interactive(&self) -> bool {
            true
        }
        fn wants_relay(&self) -> bool {
            false
        }
    }

    /// An asker that records the ask and parks it open (its surface
    /// would show a dialog); the test resolves it from outside, the
    /// way `resolve_for_chat`/`resolve` do.
    #[derive(Default)]
    struct RecordingAsker {
        seen: std::sync::Mutex<Vec<AskRequest>>,
    }

    impl RecordingAsker {
        fn ids(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.id.clone())
                .collect()
        }
    }

    #[async_trait]
    impl Asker for RecordingAsker {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            self.seen.lock().unwrap().push(req.clone());
            // A real surface registers a leg and waits; an outside
            // `resolve` ends it.
            match register(&req, None, None) {
                Some(leg) => leg.wait().await,
                None => AskAnswer::timeout(),
            }
        }
    }

    /// USER (2026-09-14): "it would be weird to have everything
    /// advertised to irc though with other people in the channel".
    ///
    /// A leg that opts out of relay receives NO bystander ask — not
    /// the prompt, not a leg in the registry — while the head (the
    /// surface that owns the session's questions) still gets
    /// everything. And an opt-out HEAD still asks: opting out of
    /// relay is not opting out of being asked. Mutation: deleting the
    /// `wants_relay()` filter in `ask_session` turns this red (the
    /// opt-out leg records the relayed prompt).
    #[tokio::test]
    async fn relay_respects_a_legs_opt_out() {
        let opt_out = Arc::new(RecordingOptOut {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let head = Arc::new(RecordingAsker::default());
        // Attach order: head LAST = most recent = the head leg.
        attach_session_asker("s-relay-opt", opt_out.clone());
        attach_session_asker("s-relay-opt", head.clone());

        let req = AskRequest::new("s-relay-opt", AskKind::Clarify, "which file?", vec![])
            .with_timeout(30);
        let h = crate::rt::spawn(async move { ask_session("s-relay-opt", req).await });

        // Give the (wrongly-relaying, if the filter is missing) spawn
        // time to run: poll the registry until the head's leg exists,
        // then one extra beat.
        let mut head_id = String::new();
        for _ in 0..100 {
            let open = open_all();
            if let Some(r) = open.iter().find(|r| r.session_id == "s-relay-opt") {
                head_id = r.id.clone();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!head_id.is_empty(), "the head leg opened the ask");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        assert!(
            head.ids().len() == 1,
            "the head leg renders the question exactly once"
        );
        assert!(
            opt_out.seen.lock().unwrap().is_empty(),
            "a relay-opt-out leg must not receive the question"
        );

        // Resolve from outside (the shape of an adapter answer); the
        // ask ends and the spawned head returns the human's answer.
        assert!(resolve(&head_id, AskAnswer::text("main.tcl")));
        let a = tokio::time::timeout(std::time::Duration::from_secs(2), h)
            .await
            .expect("ask resolves")
            .unwrap()
            .expect("a human was reachable");
        assert_eq!(a.free_text.as_deref(), Some("main.tcl"));

        // Opt-out as the HEAD: still asked, still able to answer. Its
        // `ask` returns immediately, so no resolve needed.
        let opt_head = Arc::new(RecordingOptOut {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        attach_session_asker("s-relay-opt2", opt_head.clone());
        let req2 = AskRequest::new("s-relay-opt2", AskKind::Clarify, "which file?", vec![])
            .with_timeout(30);
        let a2 = ask_session("s-relay-opt2", req2).await;
        assert_eq!(
            a2.expect("opt-out head is still asked")
                .free_text
                .as_deref(),
            Some("from the opt-out leg")
        );
        assert_eq!(opt_head.seen.lock().unwrap().len(), 1);

        clear_session_asker("s-relay-opt");
        clear_session_asker("s-relay-opt2");
    }

    // ── the ask that follows the owner off-surface ───────────────────
    //
    // PENDING §"Cross-session clarify relay". Every one of these runs on
    // `tokio::time::pause`, so the grace delay costs no wall clock and
    // the "answered in time" case is exercised by ADVANCING less than
    // the delay rather than by racing a real clock.

    /// A fallback that records what reached it and parks, so the test
    /// can see the relay arrive without ending the ask.
    #[derive(Default)]
    struct RecordingFallback {
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Asker for RecordingFallback {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            self.seen.lock().unwrap().push(req.id.clone());
            match register(&req, Some("telegram:555"), None) {
                Some(leg) => leg.wait().await,
                None => AskAnswer::timeout(),
            }
        }
    }

    /// SERIALISE. The owner fallback is ONE process-global slot, like
    /// every other registry in this file, and cargo runs these tests in
    /// parallel threads of one process: without it, test A installs its
    /// recorder, test B overwrites it, and A asserts on a recorder that
    /// was never installed when the relay fired.
    ///
    /// An ASYNC mutex, not `testenv::lock`'s std one: every test here
    /// holds the guard across `sleep().await`, and a std MutexGuard held
    /// across an await is `clippy::await_holding_lock` — correctly, since
    /// it blocks the whole worker thread rather than yielding it.
    static RELAY_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn relay_guard() -> tokio::sync::MutexGuard<'static, ()> {
        RELAY_SERIAL.lock().await
    }

    fn install_fallback(fb: Arc<RecordingFallback>, delay: u64) {
        set_owner_fallback(
            fb,
            "telegram:555",
            delay,
            vec!["approval".into(), "clarify".into(), "failover".into()],
        );
    }

    /// THE ASK. USER: "in that case I should be asked on telegram? why
    /// is nothing asking the clarify at all?" — a desktop-born clarify
    /// timed out unanswered while the owner was on Telegram.
    ///
    /// Mutation: drop the `spawn_owner_fallback` call in `ask_session`
    /// and this goes red — nothing reaches the phone, which is the bug.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_ask_follows_the_owner_to_their_phone() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        install_fallback(fb.clone(), 60);
        let head = Arc::new(RecordingAsker::default());
        attach_session_asker("s-follow", head.clone());

        let req =
            AskRequest::new("s-follow", AskKind::Clarify, "which file?", vec![]).with_timeout(600);
        let id = req.id.clone();
        let h = crate::rt::spawn(async move { ask_session("s-follow", req).await });

        // Not yet: the grace window is the whole point. A relay that
        // fires immediately buzzes the phone for every approval
        // answered at the keyboard in two seconds.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        assert!(
            fb.seen.lock().unwrap().is_empty(),
            "relayed inside the grace window"
        );

        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        assert_eq!(
            fb.seen.lock().unwrap().as_slice(),
            std::slice::from_ref(&id),
            "the unanswered question reached the owner's other surface"
        );

        // AND THE PHONE'S ANSWER RESOLVES THE DESKTOP. This is the
        // existing first-answer-wins machinery, joined by a late leg:
        // nothing new had to be built for it, which is the point.
        assert!(resolve(&id, AskAnswer::text("the second one")));
        let out = h.await.expect("join").expect("an answer");
        // `first()` reads the CHOICE keys; a free-text clarify answer
        // lives in free_text.
        assert_eq!(out.free_text.as_deref(), Some("the second one"));
        assert!(!out.timed_out, "the phone answered, nobody timed out");
        clear_session_asker("s-follow");
        clear_owner_fallback();
    }

    /// Answered at the keyboard inside the grace window: the phone
    /// stays quiet. Mutation: drop the `is_open` check in the spawned
    /// task and this goes red.
    #[tokio::test(start_paused = true)]
    async fn an_ask_answered_in_time_never_reaches_the_phone() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        install_fallback(fb.clone(), 60);
        let head = Arc::new(RecordingAsker::default());
        attach_session_asker("s-intime", head.clone());

        let req = AskRequest::new(
            "s-intime",
            AskKind::Approval,
            "run it?",
            approval_choices(true),
        )
        .with_timeout(600);
        let id = req.id.clone();
        let h = crate::rt::spawn(async move { ask_session("s-intime", req).await });
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert!(resolve(&id, AskAnswer::choice(CHOICE_ONCE)));
        let _ = h.await;

        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        assert!(
            fb.seen.lock().unwrap().is_empty(),
            "a question answered in time must leave the phone quiet"
        );
        clear_session_asker("s-intime");
        clear_owner_fallback();
    }

    /// A GUEST's approval is never routed to the owner's phone. It is
    /// the owner's attention being spent, and they would be judging
    /// somebody else's turn without the context to judge it.
    #[tokio::test(start_paused = true)]
    async fn a_guests_question_never_reaches_the_owner() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        install_fallback(fb.clone(), 60);
        let head = Arc::new(RecordingAsker::default());
        attach_session_asker("s-guest", head.clone());
        crate::acl::set_session_principal(
            "s-guest",
            crate::acl::Principal::known("irc:stranger", crate::acl::Role::Guest),
        );

        let req = AskRequest::new(
            "s-guest",
            AskKind::Approval,
            "rm -rf?",
            approval_choices(true),
        )
        .with_timeout(600);
        let h = crate::rt::spawn(async move { ask_session("s-guest", req).await });
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        assert!(
            fb.seen.lock().unwrap().is_empty(),
            "a guest's question must never reach the owner's phone"
        );
        h.abort();
        clear_session_asker("s-guest");
        clear_owner_fallback();
    }

    /// The sync decisions ask through the installed leg itself, and
    /// nothing when none is installed.
    #[tokio::test]
    async fn the_owner_leg_is_the_installed_one_or_none() {
        let _serial = relay_guard().await;
        clear_owner_fallback();
        assert!(owner_leg().is_none());
        let fb = Arc::new(RecordingFallback::default());
        install_fallback(fb.clone(), 60);
        let leg = owner_leg().expect("installed");
        let req = AskRequest::new("", AskKind::Approval, "apply?", vec![]).with_timeout(1);
        let id = req.id.clone();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), leg.ask(req)).await;
        cancel(&id);
        assert_eq!(fb.seen.lock().unwrap().len(), 1, "asked at once, no delay");
        clear_owner_fallback();
        assert!(owner_leg().is_none());
    }

    /// A kind the operator did not list stays where it was asked.
    #[tokio::test(start_paused = true)]
    async fn a_kind_the_config_did_not_list_stays_put() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        set_owner_fallback(fb.clone(), "telegram:555", 60, vec!["clarify".into()]);
        let head = Arc::new(RecordingAsker::default());
        attach_session_asker("s-kind", head.clone());

        let req = AskRequest::new(
            "s-kind",
            AskKind::Approval,
            "run it?",
            approval_choices(true),
        )
        .with_timeout(600);
        let h = crate::rt::spawn(async move { ask_session("s-kind", req).await });
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        assert!(
            fb.seen.lock().unwrap().is_empty(),
            "approval is not in the configured kinds"
        );
        h.abort();
        clear_session_asker("s-kind");
        clear_owner_fallback();
    }

    /// Born in the owner's own DM: relaying it there would ask the same
    /// human the same question twice in one window.
    #[tokio::test(start_paused = true)]
    async fn an_ask_born_in_that_very_chat_is_not_relayed_to_itself() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        install_fallback(fb.clone(), 60);
        let req = AskRequest::new("s-self", AskKind::Clarify, "which?", vec![]).with_timeout(600);
        let _leg = register(&req, Some("telegram:555"), None).expect("open");
        assert!(
            spawn_owner_fallback(&req, Some("telegram:555")).is_none(),
            "the head chat and the fallback chat are the same place"
        );
        assert!(
            spawn_owner_fallback(&req, Some("telegram:999")).is_some(),
            "…but a different chat still relays"
        );
        cancel(&req.id);
        clear_owner_fallback();
    }

    /// The head chat read at FIRE time: an ask whose own leg registered
    /// in the fallback's chat is not relayed there, even when the caller
    /// could not say where it was born (an approval asked straight
    /// through the agent's asker passes no head chat).
    #[tokio::test(start_paused = true)]
    async fn the_chat_an_ask_was_born_in_is_read_when_the_relay_fires() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        install_fallback(fb.clone(), 60);
        let req = AskRequest::new("s-born", AskKind::Approval, "run?", approval_choices(true))
            .with_timeout(600);
        let _leg = register(&req, Some("telegram:555"), None).expect("open");
        assert!(
            relay_to_owner(&req).is_some(),
            "spawned: the caller knew no chat"
        );
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        assert!(
            fb.seen.lock().unwrap().is_empty(),
            "born in the home chat: not asked there twice"
        );
        cancel(&req.id);

        // …while one born on the desktop (no chat) is relayed.
        let req = AskRequest::new("s-desk", AskKind::Approval, "run?", approval_choices(true))
            .with_timeout(600);
        let _leg = register(&req, None, None).expect("open");
        assert!(relay_to_owner(&req).is_some());
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        assert_eq!(
            fb.seen.lock().unwrap().len(),
            1,
            "a desktop approval follows the owner"
        );
        cancel(&req.id);
        clear_owner_fallback();
    }

    /// With nothing installed, every ask behaves exactly as it did.
    #[tokio::test(start_paused = true)]
    async fn no_fallback_configured_changes_nothing() {
        let _serial = relay_guard().await;
        clear_owner_fallback();
        let req = AskRequest::new("s-none", AskKind::Clarify, "which?", vec![]).with_timeout(600);
        assert!(spawn_owner_fallback(&req, None).is_none());
    }

    /// A DETACHED surface's parked relay ends, and the ask does not.
    ///
    /// The bounded zombie (PENDING 2026-09-15 audit round): a bystander
    /// relay task upgraded its Weak to a strong Arc for the whole
    /// `ask().await`, so a surface that detached while the question was
    /// parked stayed pinned until it resolved — still listed by
    /// `session_askers`, re-relayed to on the next ask, its dead adapter
    /// asked to render a dialog nobody would ever see. The fix is the
    /// per-attachment cancel handle: `detach_session_asker` fires it, the
    /// relay task's `select!` takes the detach arm, the future holding
    /// the strong Arc is dropped, and the AskLeg the surface registered
    /// goes with it (drop-self — the ask stays open for every other
    /// leg).
    ///
    /// Mutation: remove the `select!` (a bare `bystander.ask(req).await`)
    /// and the "no longer attached" assertion goes red.
    #[tokio::test]
    async fn detaching_a_surface_ends_its_parked_relay_not_the_ask() {
        // A bystander that PARKS the way real surfaces do: registers a
        // leg and waits. The test resolves (or not) from outside.
        struct ParkingBystander;
        #[async_trait]
        impl Asker for ParkingBystander {
            async fn ask(&self, req: AskRequest) -> AskAnswer {
                match register(&req, None, None) {
                    Some(leg) => leg.wait().await,
                    None => AskAnswer::timeout(),
                }
            }
        }

        let sid = "s-detach";
        let bystander = Arc::new(ParkingBystander);
        let head = Arc::new(RecordingAsker::default());
        let b_tok = attach_session_asker(sid, bystander.clone());
        let h_tok = attach_session_asker(sid, head.clone()); // head = most recent

        let req = AskRequest::new(sid, AskKind::Clarify, "which file?", vec![]).with_timeout(120);
        let id = req.id.clone();
        let h = crate::rt::spawn(async move { ask_session(sid, req).await });

        // Wait until BOTH legs are open (the bystander relay registered
        // its leg), then detach the bystander — the surface went away.
        let mut both = false;
        for _ in 0..200 {
            let legs = pending()
                .lock_ok()
                .get(&id)
                .map(|p| p.txs.len())
                .unwrap_or(0);
            if legs >= 2 {
                both = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(both, "head and bystander legs both registered");
        detach_session_asker(sid, b_tok);

        // The detached surface's leg is GONE from the ask (drop-self ran
        // when the relay task's future was cancelled), and the ask is
        // still open for the head.
        let mut settled = false;
        for _ in 0..200 {
            let legs = pending()
                .lock_ok()
                .get(&id)
                .map(|p| p.txs.len())
                .unwrap_or(0);
            if legs == 1 {
                settled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(settled, "the bystander's leg ended, the ask stays open");
        assert!(is_open(&id), "one leg (the head) keeps the ask alive");

        // And the registry no longer counts the detached surface: a
        // LATER ask on this session must not relay to it.
        assert_eq!(
            session_askers(sid).len(),
            1,
            "the detached surface is no longer an ask relay target"
        );

        // The head answers; the whole ask resolves; nothing hangs.
        assert!(resolve(&id, AskAnswer::text("main.tcl")));
        let out = tokio::time::timeout(std::time::Duration::from_secs(2), h)
            .await
            .expect("the head's answer resolves the ask")
            .unwrap()
            .expect("a human was reachable");
        assert_eq!(out.free_text.as_deref(), Some("main.tcl"));

        detach_session_asker(sid, h_tok);
    }

    // ── bot challenges (PLAN-challenge-relay.md, D2) ─────────────────

    fn bus_in(dir: &std::path::Path) -> crate::bus::GatewayBus {
        let (tx, _inbound) = tokio::sync::mpsc::unbounded_channel();
        crate::bus::GatewayBus {
            inbound: tx,
            outbound: Default::default(),
            last_chat: Default::default(),
            state_path: dir.join("gateway-state.json"),
        }
    }

    fn channel_asker(bus: &crate::bus::GatewayBus) -> ChatAsker {
        ChatAsker {
            bus: bus.clone(),
            platform: "irc".into(),
            chat_id: "#room".into(),
            sender_id: Some("~a@host".into()),
            shared_audience: true,
        }
    }

    fn challenge_req(sid: &str) -> AskRequest {
        AskRequest::new(
            sid,
            AskKind::Challenge,
            "cloudflare challenge on x.example",
            vec![
                AskChoice::new(CHOICE_CHALLENGE_SOLVED, "solved"),
                AskChoice::new(CHOICE_CHALLENGE_GIVE_UP, "give up"),
            ],
        )
        .with_detail("Open it here to click on it: https://solve.example/challenge/tok123")
        .with_timeout(60)
    }

    /// The link drives the agent's browser, so in a channel it goes to
    /// the asker alone: their query, on the network the session came
    /// from. Never the channel, and never the other network, where the
    /// same nick could be a stranger.
    #[tokio::test]
    async fn a_challenge_in_a_channel_goes_to_the_askers_query_on_its_network() {
        let (home, _env) = crate::testenv::scratch_home();
        let bus = bus_in(home.path());
        let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = tokio::sync::mpsc::unbounded_channel();
        bus.register_outbound_route("irc", "netA", a_tx);
        bus.register_outbound_route("irc", "netB", b_tx);
        let sid = "s-challenge-query";
        crate::acl::set_session_sender(
            sid,
            crate::acl::SenderContext {
                mask: Some("alice!~a@host".into()),
                network: Some("netA".into()),
                channel: Some("#room".into()),
                platform: Some("irc".into()),
                chat_id: Some("#room".into()),
                ..Default::default()
            },
        );
        let asker = channel_asker(&bus);
        let req = challenge_req(sid);
        let id = req.id.clone();
        let h = crate::rt::spawn(async move { asker.ask(req).await });
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), a_rx.recv())
            .await
            .expect("delivered")
            .expect("a message");
        assert_eq!(out.chat_id, "alice", "the asker's query, not #room");
        assert!(out.text.contains("/challenge/tok123"), "{}", out.text);
        assert!(b_rx.try_recv().is_err(), "nothing on the other network");
        // A reply typed in the query answers it.
        assert!(
            resolve_for_chat("irc:alice", "give up", Some("~a@host")).is_some(),
            "the query is where it is answered"
        );
        let a = h.await.expect("join");
        assert_eq!(a.first(), Some(CHOICE_CHALLENGE_GIVE_UP));
        assert!(!is_open(&id));
    }

    /// No way to reach the asker privately: nothing is posted in the
    /// channel (that would hand the link to everyone in it). Only this
    /// leg ends; the owner's is the one that asks.
    #[tokio::test]
    async fn a_challenge_with_no_private_way_to_the_asker_is_not_posted_in_the_channel() {
        let (home, _env) = crate::testenv::scratch_home();
        let bus = bus_in(home.path());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.register_outbound_route("irc", "", tx);
        // No session sender: no nick, no network.
        let a = channel_asker(&bus)
            .ask(challenge_req("s-challenge-noroute"))
            .await;
        assert!(a.timed_out);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "nothing reached the channel");
        // Any other kind still asks in the channel, as before.
        let req = AskRequest::new("s-challenge-noroute", AskKind::Clarify, "which?", vec![])
            .with_timeout(60);
        let id = req.id.clone();
        let asker = channel_asker(&bus);
        let h = crate::rt::spawn(async move { asker.ask(req).await });
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("delivered")
            .expect("a message");
        assert_eq!(out.chat_id, "#room");
        assert!(resolve(&id, AskAnswer::text("this one")));
        let _ = h.await;
    }

    struct CannotAsk;

    #[async_trait]
    impl Asker for CannotAsk {
        async fn ask(&self, _req: AskRequest) -> AskAnswer {
            AskAnswer::timeout()
        }
    }

    /// D2: a challenge from a session that is not the owner's reaches
    /// the owner AT ONCE — not after the fallback delay, and whatever
    /// `ask_relay_kinds` lists (here: not `challenge`). The session's
    /// own leg failing (no private route) ends only that leg.
    #[tokio::test(start_paused = true)]
    async fn a_challenge_from_someone_elses_session_reaches_the_owner_at_once() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        install_fallback(fb.clone(), 3600);
        let sid = "s-challenge-guest";
        crate::acl::set_session_principal(sid, crate::acl::Principal::guest());
        attach_session_asker(sid, Arc::new(CannotAsk));
        let req = challenge_req(sid);
        let id = req.id.clone();
        let h = crate::rt::spawn(async move { ask_challenge(sid, req).await });
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert_eq!(
            fb.seen.lock().unwrap().as_slice(),
            std::slice::from_ref(&id),
            "the owner was asked straight away"
        );
        assert!(resolve(&id, AskAnswer::choice(CHOICE_CHALLENGE_SOLVED)));
        let out = h.await.expect("join").expect("an answer");
        assert_eq!(out.first(), Some(CHOICE_CHALLENGE_SOLVED));
        clear_session_asker(sid);
        crate::acl::clear_session_principal(sid);
        clear_owner_fallback();
    }

    /// The owner's own sessions ask as any question does: their own
    /// surfaces first, the phone only after the grace window.
    #[tokio::test(start_paused = true)]
    async fn a_challenge_in_the_owners_session_waits_out_the_grace_window() {
        let _serial = relay_guard().await;
        let fb = Arc::new(RecordingFallback::default());
        set_owner_fallback(fb.clone(), "telegram:555", 60, vec!["challenge".into()]);
        let head = Arc::new(RecordingAsker::default());
        let sid = "s-challenge-owner";
        attach_session_asker(sid, head.clone());
        let req = challenge_req(sid);
        let id = req.id.clone();
        let h = crate::rt::spawn(async move { ask_challenge(sid, req).await });
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        assert!(
            fb.seen.lock().unwrap().is_empty(),
            "inside the grace window"
        );
        assert!(resolve(&id, AskAnswer::choice(CHOICE_CHALLENGE_SOLVED)));
        let out = h.await.expect("join").expect("an answer");
        assert_eq!(out.first(), Some(CHOICE_CHALLENGE_SOLVED));
        clear_session_asker(sid);
        clear_owner_fallback();
    }

    /// The owner's line in a session they joined (`crate::join`) runs
    /// with the owner's rights, so what it asks is the owner's to answer.
    /// While a turn names its asker, `ask_session` asks that one and
    /// nobody the session would otherwise ask; one that cannot ask means
    /// no human, never the session's own person. After the turn, the
    /// session's own askers are asked again.
    #[tokio::test]
    async fn a_turns_questions_go_where_the_turn_says() {
        let _serial = relay_guard().await;
        struct Says(&'static str, std::sync::Mutex<usize>);
        #[async_trait]
        impl Asker for Says {
            async fn ask(&self, _req: AskRequest) -> AskAnswer {
                *self.1.lock().unwrap() += 1;
                AskAnswer::text(self.0)
            }
        }
        let sid = "s-turn-asker";
        let user = Arc::new(Says("the user", std::sync::Mutex::new(0)));
        let owner = Arc::new(Says("the owner", std::sync::Mutex::new(0)));
        let _held = attach_session_asker(sid, user.clone());
        let ask = || AskRequest::new(sid, AskKind::Approval, "run it?", approval_choices(true));

        set_turn_asker(sid, owner.clone());
        let a = ask_session(sid, ask()).await.expect("the owner answers");
        assert_eq!(a.free_text.as_deref(), Some("the owner"));
        assert_eq!(*owner.1.lock().unwrap(), 1);
        assert_eq!(
            *user.1.lock().unwrap(),
            0,
            "the session's person is not asked"
        );

        set_turn_asker(sid, Arc::new(PolicyAsker { approve: false }));
        assert!(
            ask_session(sid, ask()).await.is_none(),
            "an owner who cannot be asked is no human, not the session's person"
        );
        assert_eq!(*user.1.lock().unwrap(), 0);

        clear_turn_asker(sid);
        let a = ask_session(sid, ask()).await.expect("the user answers");
        assert_eq!(a.free_text.as_deref(), Some("the user"));
        clear_session_asker(sid);
    }
}
