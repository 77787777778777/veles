//! delegate_task tool — subagents as one-shot `veles chat -q` children.
//!
//! hermes parity target: async_delegation.py. Design differences worth
//! noting (the "better design" pass):
//!   - **Subprocess, not in-process task.** Each child is a fresh
//!     `veles chat -q -Q` process: own session, own Tcl interp, own
//!     provider client. A wedged child cannot jam the parent's event
//!     loop; killing the parent orphans nothing shared.
//!   - **Recursion guard by construction.** Children inherit
//!     VELES_CHILD_DEPTH=1; the tool refuses when depth ≥ 1. hermes
//!     enforces nesting limits with config + prompt.
//!   - **Batches run truly parallel** (up to 3) and the tool returns ONE
//!     consolidated result — same contract as hermes delegate_task.
//!   - **Background mode (S3, USER-requested 2026-08-22)**: background
//!     = true returns immediately with a child id; when the child
//!     finishes, its summary is injected into the PARENT SESSION's next
//!     turn boundary as a [BACKGROUND CHILD COMPLETE] message (the
//!     steer path, generalized). Completion fires subagent_start,
//!     subagent_stop and background_child_complete when a hook bus is
//!     attached -- for years this line claimed a hook fired and none
//!     did (hooks.rs kept all three in RESERVED_HOOKS).
//!   - **Self-report caveat preserved in the description**: child
//!     summaries are claims, not verified facts (hermes carries the same
//!     warning in its tool description).

use crate::lockutil::LockOk;
use crate::providers::CancelToken;
use crate::tasks::TaskKind;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Hard wall-clock cap for one child (soft in spirit: we kill and report,
/// the way hermes reports child timeouts rather than hanging forever).
pub const CHILD_TIMEOUT_SECS: u64 = 900;
const MAX_BATCH: usize = 3;
const OUTPUT_CAP: usize = 8_000;

/// Why a child stopped without exiting on its own.
enum Stop {
    Timeout,
    Cancelled,
}

pub struct DelegateTaskTool {
    /// Parent session id — background children inject their completion
    /// into this session's turn boundary (S3).
    pub session_id: String,
    /// Hook bus for subagent_start / subagent_stop /
    /// background_child_complete. Optional so tests and callers without
    /// a bus can still build the tool.
    pub hooks: Option<std::sync::Arc<crate::hooks::HookBus>>,
}

impl DelegateTaskTool {
    /// The bus to fire on: the explicit one if set, else the process
    /// bus published by `Agent::new`.
    fn bus(&self) -> Option<std::sync::Arc<crate::hooks::HookBus>> {
        self.hooks.clone().or_else(crate::hooks::default_bus)
    }

    /// B4: where does this child run? `None` = unset (this node, the
    /// historical default). Resolves against the LIVE registry; a
    /// resolution error is returned verbatim (honest errors, never a
    /// silent local fallback — a placement constraint that quietly ran
    /// the work elsewhere is the failure it existed to prevent).
    ///
    /// `needs` (the call's `needs` array, PLAN-phone-userland.md T2):
    /// programs the chosen node must advertise; empty constrains nothing.
    fn resolve_backend(
        &self,
        spec: Option<&crate::placement::BackendSpec>,
        needs: &[String],
    ) -> Result<Option<crate::placement::Placement>, String> {
        let Some(spec) = spec else { return Ok(None) };
        let home = crate::config::veles_home();
        let store = match crate::store::Store::open(&home.join("state.db")) {
            Ok(s) => s,
            Err(e) => return Err(format!("cannot open the registry: {e:#}")),
        };
        let stale = crate::config::load()
            .map(|c| c.cluster.stale_after_secs)
            .unwrap_or(600) as i64;
        let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
            .ok()
            .flatten()
            .map(|i| i.id);
        let routable = |n: &crate::store::NodeRow| {
            me.as_deref() == Some(n.id.as_str()) || remote_target_for(n).is_some()
        };
        crate::placement::resolve_needing(&store, spec, me.as_deref(), stale, &routable, needs)
            .map(Some)
            .map_err(|e| format!("{e:#}"))
    }

    /// B4: run one child on its placed node. Local placements (and no
    /// placement at all) return Ok(None) — the caller runs the normal
    /// local path; a REMOTE placement performs the cluster hop and
    /// returns the peer's verdict JSON.
    #[allow(clippy::too_many_arguments)]
    async fn run_remote_if_placed(
        &self,
        spec: Option<&crate::placement::BackendSpec>,
        needs: &[String],
        goal: &str,
        ctx: &str,
        model: Option<&str>,
        provider: Option<&str>,
        reasoning: Option<&str>,
    ) -> Result<Option<Value>, String> {
        let placement = match self.resolve_backend(spec, needs)? {
            Some(p) => p,
            // No placement ask: local path, nothing to do here.
            None => return Ok(None),
        };
        let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
            .ok()
            .flatten()
            .map(|i| i.id);
        if crate::placement::is_local(&placement, me.as_deref()) {
            // Resolved here but landed local (a one-node pool, or
            // `any` picked us): the local caller path takes it.
            return Ok(None);
        }
        let ask = self.remote_ask(
            goal,
            ctx,
            model.map(str::to_string),
            provider.map(str::to_string),
            reasoning.map(str::to_string),
        );
        // C3: a foreground delegate runs inside a turn, so the Stop
        // button (agent::cancel_turn on THIS session) is the watch.
        let watch = crate::remotetask::CancelWatch::turn(&self.session_id);
        run_on_placed(&placement, &ask, watch).await.map(Some)
    }

    /// The ask a remote child is started from, carrying the CALLER's
    /// principal (serialized exactly as a local child's env gets it).
    /// Without it the peer ran the child as owner.
    fn remote_ask(
        &self,
        goal: &str,
        ctx: &str,
        model: Option<String>,
        provider: Option<String>,
        reasoning: Option<String>,
    ) -> crate::remotetask::RemoteTaskAsk {
        crate::remotetask::RemoteTaskAsk {
            from_node: crate::node::NodeIdentity::load(&crate::config::config_dir())
                .ok()
                .flatten()
                .map(|i| i.id)
                .unwrap_or_default(),
            goal: goal.to_string(),
            context: ctx.to_string(),
            model,
            provider,
            reasoning,
            principal: crate::acl::session_principal(&self.session_id)
                .and_then(|p| serde_json::to_string(&*p).ok()),
            // C3: the receiver registers its A2A task under THIS id
            // (WORKING, before the child starts) so the sender's
            // cancel watcher has a handle to aim tasks/cancel at.
            wire_task_id: Some(crate::a2a::new_task_id()),
            ticket: None,
        }
    }
}

/// One remote hop to a placed node: dial it through its `[a2a_agents]`
/// entry, send the ask, and return the peer's verdict marked with where
/// it ran. Errors are honest and name the node — never a local rerun.
async fn run_on_placed(
    p: &crate::placement::Placement,
    ask: &crate::remotetask::RemoteTaskAsk,
    watch: crate::remotetask::CancelWatch,
) -> Result<Value, String> {
    // The registry `endpoint` is information; the DIALING config is
    // [a2a_agents] (auth lives there and nowhere else).
    let (url, bearer) = remote_target_for(&p.node).ok_or_else(|| {
        format!(
            "backend node '{}' ({}) has no [a2a_agents] entry — add one with its \
             url + auth token to route work to it",
            p.node.name, p.node.id
        )
    })?;
    let mut v = crate::remotetask::run_on_peer(&url, bearer.as_deref(), ask, watch)
        .await
        .map_err(|e| format!("remote backend {}: {e:#}", p.node.name))?;
    v["backend"] = json!(format!("{} ({}) — {}", p.node.name, p.node.id, p.reason));
    Ok(v)
}

/// How deep in the subagent tree this process is. 0 = the operator's
/// own surface; 1 = a child, which may not spawn further children.
///
/// Public because the guard has TWO halves that must agree: the
/// tool's `call` refuses (unbypassable, whatever advertises it), and
/// `tools::builtin_registry` does not advertise it in the first place.
pub fn child_depth() -> u32 {
    child_env_depth()
}

fn child_env_depth() -> u32 {
    std::env::var("VELES_CHILD_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

// ── background children (S3) ─────────────────────────────────────────
//
// Completion events queue here, keyed by the PARENT session id. The
// turn loop (agent.rs) drains its session's events at every turn
// boundary — the same injection point as /steer, so a completed child
// wakes the conversation even mid-turn.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackgroundEvent {
    pub child_id: u32,
    /// Which kind of task finished — picks the banner the model sees.
    pub kind: crate::tasks::TaskKind,
    pub goal: String,
    pub result: Value,
    /// For a Subagent: the child SESSION it ran in, so the injection can
    /// name where the full transcript lives ("read the rest in session
    /// X"). None for shells and monitors.
    pub child_session: Option<String>,
    #[serde(with = "serde_systemtime")]
    pub finished_at: std::time::SystemTime,
}

/// SystemTime ↔ epoch-seconds serde, for persisting background events
/// across a process restart (the gateway's idle exit on the phone, a
/// deploy bounce on the VPS).
mod serde_systemtime {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let secs = t
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        secs.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

static BACKGROUND_EVENTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<BackgroundEvent>>>,
> = std::sync::OnceLock::new();

/// One short process lock orders the two halves of a queue operation.
/// It is held only while the in-memory dock is changed and its matching
/// durable rows are drained; no provider call or user code runs here.
fn background_queue_lock() -> std::sync::MutexGuard<'static, ()> {
    static QUEUE: OnceLock<Mutex<()>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(())).lock_ok()
}

fn background_events(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<BackgroundEvent>>> {
    BACKGROUND_EVENTS.get_or_init(|| std::sync::Mutex::new(Default::default()))
}

// ── durable backing ────────────────────────────────────────────────
//
// The in-memory map above is the fast path; the store is the durable
// one. A background event parked in memory only is lost if the process
// exits between the child finishing and the next turn — the phone's
// idle exit hits this every night. Same set_store + fallback pattern
// as `dock_steer` in ask.rs: the store is optional, tests and headless
// one-shots use the in-memory dock alone.

static BG_STORE: OnceLock<std::sync::RwLock<Option<Arc<Mutex<crate::store::Store>>>>> =
    OnceLock::new();

pub fn set_bg_store(store: Arc<Mutex<crate::store::Store>>) {
    if let Ok(mut w) = BG_STORE
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
    {
        *w = Some(store);
    }
}

fn bg_store() -> Option<Arc<Mutex<crate::store::Store>>> {
    BG_STORE.get()?.read().ok()?.clone()
}

/// Serialize one event into its session_queue body. Kind tag
/// 'bgevent' keeps it apart from messages, steers and mail.
fn event_to_body(ev: &BackgroundEvent) -> Option<String> {
    serde_json::to_string(ev).ok()
}

fn event_from_body(body: &str) -> Option<BackgroundEvent> {
    serde_json::from_str(body).ok()
}

pub fn push_background_event(session_id: &str, ev: BackgroundEvent) {
    push_background_event_to(session_id, ev, bg_store());
}

fn push_background_event_to(
    session_id: &str,
    ev: BackgroundEvent,
    store: Option<Arc<Mutex<crate::store::Store>>>,
) {
    let _queue = background_queue_lock();
    // Durable first (the store), then the in-memory fast path.
    if let Some(store) = store {
        if let Some(body) = event_to_body(&ev) {
            let _ = store
                .lock_ok()
                .queue_push_kind(session_id, &body, "bgevent");
        }
    }
    background_events()
        .lock_ok()
        .entry(session_id.to_string())
        .or_default()
        .push(ev);
}

/// May this session start a wake turn? Non-destructive: the waker uses
/// this to decide whether a provider call would have anything to say,
/// and the turn itself drains for real (atomically) inside.
pub fn has_background_events(session_id: &str) -> bool {
    // In-memory fast path first.
    if background_events()
        .lock_ok()
        .get(session_id)
        .is_some_and(|v| !v.is_empty())
    {
        return true;
    }
    // Then the durable dock — a previous process may have left rows.
    if let Some(store) = bg_store() {
        return store
            .lock_ok()
            .queue_items(session_id)
            .map(|items| {
                items
                    .iter()
                    .any(|i| i.kind.as_deref() == Some("bgevent") && i.state == "pending")
            })
            .unwrap_or(false);
    }
    false
}

// ── the wake claim ──────────────────────────────────────────────────
//
// Two wake paths exist in one process (the gateway's, for chat-backed
// sessions; the desktop server's, for sessions it owns), and both
// subscribe to the same session bus — so one `shell-complete` could
// start TWO turns on one session. The claim is a process-wide
// one-at-a-time gate: the first waker to claim runs the turn; every
// other sees a held claim and stands down. The guard releases on drop,
// so a turn that panics cannot leave the session permanently unwakable.

static WAKE_CLAIMS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

fn wake_claims() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    WAKE_CLAIMS.get_or_init(|| std::sync::Mutex::new(Default::default()))
}

/// Try to claim this session's wake turn. `None` when another waker
/// (or a previous, still-running wake) already holds it.
pub fn try_claim_wake(session_id: &str) -> Option<WakeClaim> {
    let mut set = wake_claims().lock_ok();
    if set.insert(session_id.to_string()) {
        Some(WakeClaim {
            session_id: session_id.to_string(),
            retry_on_drop: false,
        })
    } else {
        None
    }
}

/// Held for the whole wake turn; dropping it is what frees the next
/// completion to wake the session again.
pub struct WakeClaim {
    session_id: String,
    retry_on_drop: bool,
}

impl WakeClaim {
    /// Arm only once the owner has passed liveness, injection-mute,
    /// lock and budget admission and is about to run the wake turn.
    /// Retrying a muted or budget-refused session would spin forever
    /// without ever draining its deliberately parked events.
    pub fn retry_pending_on_drop(&mut self) {
        self.retry_on_drop = true;
    }
}

impl Drop for WakeClaim {
    fn drop(&mut self) {
        let _ = wake_claims().lock_ok().remove(&self.session_id);
        // A completion during the wake's own final response found this
        // claim held and stood down. Retry only AFTER releasing it;
        // otherwise two shells finishing one wake apart strand the
        // second until the user's next message.
        if self.retry_on_drop && has_background_events(&self.session_id) {
            crate::session_tap::publish(
                &self.session_id,
                "background-wake-pending",
                serde_json::json!({ "session_id": self.session_id }),
            );
        }
    }
}

/// Take (drain) all pending completion events for a session.
/// Drop a finished background SHELL's queued completion — for when the
/// model already received the same output from a blocking
/// `shell{action:wait}`, so the turn boundary does not repeat it.
/// Returns whether one was queued.
pub fn discard_shell_completion(session_id: &str, shell_id: u32) -> bool {
    discard_shell_completion_from(session_id, shell_id, bg_store())
}

fn discard_shell_completion_from(
    session_id: &str,
    shell_id: u32,
    store: Option<Arc<Mutex<crate::store::Store>>>,
) -> bool {
    let _queue = background_queue_lock();
    let mut found = false;
    if let Some(store) = store {
        match store
            .lock_ok()
            .discard_bgevent(session_id, shell_id, "Shell")
        {
            Ok(durable) => found |= durable,
            Err(e) => tracing::warn!(
                "background events: discarding shell {shell_id}'s durable row failed: {e:#}"
            ),
        }
    }
    let mut all = background_events().lock_ok();
    let Some(v) = all.get_mut(session_id) else {
        // A durable recovery row can exist after the memory copy is
        // gone; `found` above is the answer in that case too.
        return found;
    };
    let before = v.len();
    v.retain(|e| !(e.kind == crate::tasks::TaskKind::Shell && e.child_id == shell_id));
    found |= v.len() != before;
    if v.is_empty() {
        all.remove(session_id);
    }
    found
}

/// Drain every queued background event for this session, oldest first.
///
/// A completion normally exists in BOTH docks: the durable row is its
/// crash-recovery copy and the in-memory row is its fast-path copy. They
/// are one event, not two. Reconcile their serialized bodies so a wake
/// cannot persist or bill for the same completion twice, while a row
/// left by an earlier process and a memory-only event (no store, or a
/// failed durable write) still get through.
pub fn take_background_events(session_id: &str) -> Vec<BackgroundEvent> {
    take_background_events_from(session_id, bg_store())
}

/// The store-explicit half, separated so the recovery tests do not have
/// to mutate the process-global dock used by other tests.
fn take_background_events_from(
    session_id: &str,
    store: Option<Arc<Mutex<crate::store::Store>>>,
) -> Vec<BackgroundEvent> {
    let _queue = background_queue_lock();
    take_background_events_locked(session_id, store)
}

fn take_background_events_locked(
    session_id: &str,
    store: Option<Arc<Mutex<crate::store::Store>>>,
) -> Vec<BackgroundEvent> {
    if let Some(store) = store {
        match store.lock_ok().take_bgevents(session_id) {
            Ok(rows) => {
                // Remove this process's fast-path copy BEFORE returning
                // anything. The queue lock keeps a new push from entering
                // between that removal and the matching below.
                let mut memory = background_events()
                    .lock_ok()
                    .remove(session_id)
                    .unwrap_or_default();
                let mut recovery = rows.clone();
                let mut out = Vec::new();
                for ev in memory.drain(..) {
                    let body = event_to_body(&ev);
                    if let Some(pos) = body
                        .as_ref()
                        .and_then(|b| recovery.iter().position(|row| row == b))
                    {
                        // A mirrored event is one delivery, not two.
                        // Preserve its live-queue position and remove
                        // only the matching durable recovery copy.
                        recovery.remove(pos);
                    }
                    // A memory-only event (no durable twin) is kept too.
                    out.push(ev);
                }
                out.extend(recovery.iter().filter_map(|body| event_from_body(body)));
                return out;
            }
            Err(e) => {
                // The in-memory dock is still authoritative for what this
                // process saw. Leave durable rows in place on a failed
                // read/delete rather than pretending they were consumed.
                tracing::warn!("background events: durable drain failed for {session_id}: {e:#}");
            }
        }
    }
    background_events()
        .lock_ok()
        .remove(session_id)
        .unwrap_or_default()
}

/// The binary running us is the binary children run as (dev build → dev
/// children, installed build → installed children). `VELES_BINARY`
/// overrides for tests / protocol labs pointing at a specific install.
fn veles_binary() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("VELES_BINARY") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot resolve own binary path: {e}"))
}

fn cap_output(s: &str) -> String {
    if s.chars().count() <= OUTPUT_CAP {
        s.to_string()
    } else {
        let cut: String = s.chars().take(OUTPUT_CAP).collect();
        format!(
            "{cut}\n… [truncated {} chars]",
            s.chars().count() - OUTPUT_CAP
        )
    }
}

/// Mint the child SESSION before the child process exists (USER
/// taxonomy, 2026-09-17: subagents are "child sessions within veles
/// … available to attach and review from the session list", and /btw
/// is "forked off the parent session").
///
/// The row is created here, in the PARENT's process, against the same
/// state.db the child will open: parent link, title, and — for a fork —
/// a copy of the parent's history. Then `run_child_routed` hands the id
/// to the child as `--resume <id>`, so the child boots INTO a session
/// that already exists and already knows where it came from, instead of
/// minting its own anonymous one the parent never hears the id of.
///
/// `fork_history` is the /btw half of the taxonomy: a btw child answers
/// a question ABOUT this conversation, so it inherits the transcript.
/// A `delegate_task` child does NOT — its contract is "children know
/// nothing of this conversation, pass everything needed via context" —
/// so it forks the LINK but not the rows.
///
/// Returns None if the store cannot be opened or the row cannot be
/// written: a spawn must not fail because the session list could not be
/// prettified, so the child falls back to minting its own session.
fn provision_child_session(parent: &str, goal: &str, fork_history: bool) -> Option<String> {
    let home = crate::config::veles_home();
    let store = crate::store::Store::open(&home.join("state.db")).ok()?;
    let child = crate::store::new_session_id();
    // The title is the goal, capped: it is what the session list shows,
    // and a 4kB prompt as a title is unreadable. "subagent:" marks it as
    // a child at a glance even before the parent column is rendered.
    let title: String = goal.chars().take(60).collect();
    let title = format!("subagent: {title}");
    store
        .create_child_session(&child, parent, Some(&title))
        .ok()?;
    if fork_history {
        // Best-effort: a child that boots with an empty fork still runs
        // and still reports; the fork is context, not a precondition.
        let _ = store.copy_history(parent, &child);
    }
    Some(child)
}

/// The gateway URL a child should relay its session events to.
///
/// Loopback and the configured desktop port: the gateway is a
/// single-box service and the child runs on that same box. Computed
/// rather than discovered — `default_port` resolves the same number
/// the gateway binds, so there is nothing to look up and nothing to go
/// stale.
///
/// An already-set `VELES_RELAY_TO` means an operator (or a test) chose
/// a target; honour it rather than overwrite it.
fn relay_target_for_children() -> Option<String> {
    if let Some(explicit) = crate::relay::target() {
        return Some(explicit);
    }
    let port = crate::config::desktop_port(&crate::config::live());
    Some(format!("http://127.0.0.1:{port}"))
}

/// B4: how to DIAL a placed node. The registry row's `endpoint` is
/// information; routing work needs auth, and auth lives in
/// `[a2a_agents]` — so a placement resolves to an agents entry or the
/// work does not move. Resolution order: exact id match, unique id
/// prefix, exact name. pub(crate): B6 migration dials a target node
/// the same way placement does.
pub(crate) fn remote_target_for(node: &crate::store::NodeRow) -> Option<(String, Option<String>)> {
    let cfg = crate::config::load().ok()?;
    // The agents table keys are NAMES; the registry carries ids.
    // Endpoint match first (the operator's two tables agree on the
    // same URL), then name match (the common spelling: agent name ==
    // node name).
    for (name, a) in cfg.a2a_agents.iter() {
        if let Some(ep) = &node.endpoint {
            if a.url.trim_end_matches('/') == ep.trim_end_matches('/') {
                let bearer = {
                    let t = a.auth.token.trim();
                    (!t.is_empty()).then(|| t.to_string())
                };
                return Some((a.url.clone(), bearer));
            }
        }
        if name == &node.name {
            let bearer = {
                let t = a.auth.token.trim();
                (!t.is_empty()).then(|| t.to_string())
            };
            return Some((a.url.clone(), bearer));
        }
    }
    None
}

/// The route a session is pinned to, or an empty one.
///
/// `session_overrides` is where every surface's `/model` lands
/// (`Agent::persist_overrides`) and where every agent opening a session
/// reads it back (`Agent::new`), so it is the one cross-process answer
/// to "what is this session running on". No row, or an empty one, means
/// the session is itself on `[model] default` — and then the child
/// should be too, which is what an empty override says.
///
/// Best-effort: a store that will not open must not stop a `/btw`.
fn parent_route(parent: &str) -> crate::store::SessionOverride {
    let home = crate::config::veles_home();
    crate::store::Store::open(&home.join("state.db"))
        .ok()
        .and_then(|s| s.override_get(parent).ok().flatten())
        .unwrap_or_default()
}

/// A child session that never got a single message is debris, not
/// history.
///
/// The row is minted before the process exists so `/subagents` can
/// offer "attach" from the moment the task does — but a spawn that
/// never started has nothing to attach to, and on 2026-09-18 three
/// such rows were sitting in the user's session list, one per
/// `delegate_task` call that died in clap: titled, parent-linked, zero
/// messages, `created_at == updated_at`.
///
/// Reaped ONLY when empty. A child that ran and then failed keeps its
/// transcript — being able to read what a failed child did is the
/// whole reason the session is minted early. Best-effort throughout:
/// a store that will not open must not turn a failed spawn into a
/// worse failure.
fn reap_empty_child(child: Option<&str>) {
    let Some(cs) = child else { return };
    let home = crate::config::veles_home();
    let Ok(store) = crate::store::Store::open(&home.join("state.db")) else {
        return;
    };
    if store.session_message_count(cs).unwrap_or(1) == 0 {
        let _ = store.delete_session(cs);
    }
}

/// Run one child synchronously (called on a spawn_blocking thread — plain
/// blocking std::process, no nested runtime).
#[cfg(all(test, unix))]
fn run_child(goal: &str, context: &str, cancel: Option<CancelToken>) -> Value {
    run_child_routed(goal, context, None, None, None, "", None, cancel)
}

/// Run a REMOTE peer's child locally (B4 placement): the a2a_server's
/// remote-task handler calls this on a blocking thread. It is the same
/// runner and the same child contract as a local delegate — one code
/// path — with one remote-specific difference: the child session is
/// minted HERE (titled `remote: <goal>`, parented to the pseudo
/// session `a2a-remote`) because the transcript belongs to the node
/// that ran it, and its id rides back as `remote_child_session` so
/// the sender's verdict names something real instead of a pointer
/// into a store it cannot read (until B5 replicates it).
pub fn run_remote_child(
    goal: &str,
    context: &str,
    model: Option<&str>,
    provider: Option<&str>,
    reasoning: Option<&str>,
    // The CALLER's principal, serialized on the placing node. It used to
    // be dropped here and the child parented to the principal-free
    // pseudo-session `a2a-remote`, i.e. run as OWNER: a Trusted caller
    // (Trusted holds Delegate) got Cap::All on the other node, secrets
    // included. One that does not parse is refused, never widened.
    principal: Option<&str>,
    // C3: the receiver's live cancel token (flipped by tasks/cancel on
    // the wire id the sender minted). Threaded into the same kill loop
    // a LOCAL child's token rides, so a remote cancel kills the process
    // rather than merely relabeling a registry row. None = no cancel
    // handle (an ask from an older sender); the child then runs to its
    // own cap exactly as B4 shipped it.
    cancel: Option<CancelToken>,
) -> Value {
    if let Some(p) = principal {
        if let Err(e) = serde_json::from_str::<crate::acl::Principal>(p) {
            return json!({
                "status": "error",
                "error": format!("refused: the caller's principal does not parse ({e})")
            });
        }
    }
    let home = crate::config::veles_home();
    let child_session = crate::store::Store::open(&home.join("state.db"))
        .ok()
        .and_then(|s| {
            let id = crate::store::new_session_id();
            let title = format!("remote: {}", goal.chars().take(40).collect::<String>());
            s.create_child_session(&id, "a2a-remote", Some(&title))
                .ok()
                .map(|_| id)
        });
    let mut v = run_child_as(
        goal,
        context,
        model,
        provider,
        reasoning,
        principal.map(str::to_string),
        child_session.as_deref(),
        cancel,
    );
    if let Some(cs) = &child_session {
        v["remote_child_session"] = json!(cs);
    }
    v
}

/// The child's argv, as one function, because the parser lives in
/// another crate and could not see it.
///
/// `-q` IS `--query` and takes a VALUE. Any flag inserted between it and
/// the prompt makes clap refuse the whole command line before the child
/// boots — which is what `39c1ed3` (`-m`/`-p`/`-r`) and `c0c12c6`
/// (`--resume`, unconditional) each did, and what no test could catch:
/// every spawn test points `VELES_BINARY` at a shell script, so the argv
/// was asserted for CONTENT and never fed to a parser.
///
/// THE PROMPT GOES DIRECTLY AFTER `-q`. Everything else follows it.
/// `veles-cli`'s `argv_tests::argv_the_real_parser_accepts` runs
/// `Cli::try_parse_from` over exactly this, and
/// `tests/child_spawn_reaches_the_model.rs` runs the real binary over
/// it — so builder and parser check each other instead of a fake.
pub fn child_argv(
    prompt: &str,
    child_session: Option<&str>,
    model: Option<&str>,
    provider: Option<&str>,
    reasoning: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["chat".to_string(), "-q".to_string(), prompt.to_string()];
    if let Some(cs) = child_session {
        // Boot INTO the pre-minted session row: same id the parent's
        // listing and the session list show, so "attach and review" has
        // a real target from the moment the task exists.
        argv.push("--resume".into());
        argv.push(cs.to_string());
    }
    if let Some(m) = model {
        argv.push("-m".into());
        argv.push(m.to_string());
    }
    if let Some(p) = provider {
        argv.push("-p".into());
        argv.push(p.to_string());
    }
    if let Some(r) = reasoning {
        argv.push("-r".into());
        argv.push(r.to_string());
    }
    argv.push("-Q".into());
    argv
}

/// Run one child synchronously with optional routing overrides (S7
/// agent presets). Called on a spawn_blocking thread.
// Eight flat parameters, allowed like `Store::cron_update` rather than
// bundled: every caller spells all of them positionally and the three
// route overrides must stay independent (an alias may pin reasoning
// without pinning a model). NOTE: this allow landed with the Stage 5
// gate run, not with the Stage 3 commit that grew the 8th argument —
// the gate would have caught it, and did, the first time it ran after.
#[allow(clippy::too_many_arguments)]
fn run_child_routed(
    goal: &str,
    context: &str,
    model: Option<&str>,
    provider: Option<&str>,
    reasoning: Option<&str>,
    // The PARENT session id: whose principal the child inherits.
    parent_session: &str,
    // The child SESSION, pre-minted by `provision_child_session` (row +
    // parent link + title already in state.db). The child resumes INTO
    // it, so the session list shows a real titled child the user can
    // attach to. None falls back to the child minting its own.
    child_session: Option<&str>,
    // Flipped by `/subagents cancel` or the API; the poll loop below
    // turns it into a real kill.
    cancel: Option<CancelToken>,
) -> Value {
    // The child must answer to the SAME principal as the caller. A bare
    // `veles chat` IS the owner surface (that is what the CLI is: the
    // human's own half), so a delegate child spawned without this would
    // be an owner-equivalent subagent reachable from a trusted+ gate —
    // the exact hole the media and F1 gates cannot bind, because no
    // principal means no gate. Serialized here, re-bound in the chat
    // boot path (`acl::principal_from_env`). When the caller HAS no
    // principal (an owner CLI turn delegating), no var: same human,
    // same surface.
    let principal_env =
        crate::acl::session_principal(parent_session).and_then(|p| serde_json::to_string(&*p).ok());
    run_child_as(
        goal,
        context,
        model,
        provider,
        reasoning,
        principal_env,
        child_session,
        cancel,
    )
}

/// [`run_child_routed`] with the principal already serialized: the
/// local path derives it from the parent session, a REMOTE child (B4)
/// takes the one that travelled with the ask — the caller's, from the
/// node that placed the work. `None` = the caller had no principal (an
/// owner surface).
#[allow(clippy::too_many_arguments)]
fn run_child_as(
    goal: &str,
    context: &str,
    model: Option<&str>,
    provider: Option<&str>,
    reasoning: Option<&str>,
    principal_env: Option<String>,
    child_session: Option<&str>,
    cancel: Option<CancelToken>,
) -> Value {
    let bin = match veles_binary() {
        Ok(b) => b,
        Err(e) => return json!({"status": "error", "error": e.to_string()}),
    };
    // Child prompt: context first (background), then the goal as the task.
    let mut prompt = String::new();
    if !context.is_empty() {
        prompt.push_str(&format!("BACKGROUND / CONTEXT:\n{context}\n\n"));
    }
    prompt.push_str(&format!("TASK:\n{goal}\n\n"));
    prompt.push_str(
        "You are a subagent: finish the task and end with a final summary. \
         You cannot spawn further subagents and cannot ask the user questions.",
    );

    let argv = child_argv(&prompt, child_session, model, provider, reasoning);

    let mut command = std::process::Command::new(&bin);
    command
        .args(&argv)
        .env("VELES_CHILD_DEPTH", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(pe) = &principal_env {
        command.env(crate::acl::PRINCIPAL_ENV, pe);
    }
    // Where to send the events this child's own bus will carry.
    //
    // `session_tap` is process-wide and this is a separate process, so
    // without this the child streams a full turn — deltas, reasoning,
    // tool rounds — to a bus with no subscribers, and a window
    // attached to the child sees the store replay and then nothing
    // (USER 2026-09-18). `crate::relay` reads this at the child's boot
    // and posts to the gateway, which republishes onto the bus the
    // windows are watching.
    //
    // Set on the CHILD only, never on the gateway: that asymmetry is
    // the entire no-loop guarantee.
    if let Some(url) = relay_target_for_children() {
        command.env(crate::relay::RELAY_ENV, url);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            reap_empty_child(child_session);
            return json!({"status": "error", "error": format!("spawn failed: {e}")});
        }
    };

    // Drain both pipes on helper threads so a chatty child can't deadlock
    // on a full pipe buffer while we wait.
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let out_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stdout_pipe, &mut buf).ok();
        buf
    });
    let err_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stderr_pipe, &mut buf).ok();
        buf
    });

    // Poll for exit against the wall-clock cap.
    let deadline = std::time::Instant::now() + Duration::from_secs(CHILD_TIMEOUT_SECS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => {
                // Cancellation rides the loop that already enforces the
                // deadline, and reuses the same kill. A cancel that only
                // relabelled the registry row would leave the child
                // running to its 900s cap -- which is exactly what a2a's
                // tasks/cancel does, and why this is checked here.
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(Stop::Cancelled);
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(Stop::Timeout);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return json!({"status": "error", "error": format!("wait failed: {e}")}),
        }
    };
    let stdout = String::from_utf8_lossy(&out_t.join().unwrap_or_default())
        .trim()
        .to_string();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default())
        .trim()
        .to_string();
    let stderr_tail: String = stderr
        .chars()
        .rev()
        .take(500)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    // Every verdict names the child session when there is one: the
    // tool description promises the full transcript is reviewable, and
    // the BLOCKING path's only channel to the model is this JSON. The
    // id lets the model (and through it the user) /resume or
    // session_search the child instead of trusting the summary alone.
    let session_field = child_session
        .map(|cs| json!(cs))
        .unwrap_or(serde_json::Value::Null);
    match status {
        Ok(s) if s.success() => json!({
            "status": "ok",
            "summary": cap_output(&stdout),
            "stderr_tail": cap_output(&stderr_tail),
            "child_session": session_field,
        }),
        Ok(s) => {
            // A child that exited non-zero having written nothing never
            // got as far as a turn. Its pre-minted session is debris —
            // but only if it is EMPTY; see `reap_empty_child`. NOT done
            // for Timeout or Cancelled below: those children ran.
            reap_empty_child(child_session);
            let stdout_tail: String = stdout
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            json!({
                "status": "error",
                "error": format!("child exited {:?}", s.code()),
                "stdout_tail": cap_output(&stdout_tail),
                "stderr_tail": cap_output(&stderr_tail),
                "child_session": session_field,
            })
        }
        Err(Stop::Timeout) => json!({
            "status": "error",
            "error": format!("child timed out after {CHILD_TIMEOUT_SECS}s"),
            "stderr_tail": cap_output(&stderr_tail),
            "child_session": session_field,
        }),
        // Distinct from "error": the caller asked for this, and the
        // registry reports the task Cancelled rather than failed.
        Err(Stop::Cancelled) => json!({
            "status": "cancelled",
            "error": "cancelled by request",
            "stderr_tail": cap_output(&stderr_tail),
            "child_session": session_field,
        }),
    }
}

/// Spawn a tracked background child and return its task id.
///
/// Shared by the `delegate_task` tool's background mode and the `/btw`
/// slash command in every frontend, so the two cannot drift — the old
/// `/background` was a separate, worse implementation that resumed the
/// CALLER's session and discarded the output.
///
/// The child SESSION is minted BEFORE the child process (Stage 3, USER
/// taxonomy 2026-09-17): titled, parent-linked, listed in the session
/// list, and stamped onto the registry row so `/subagents` and
/// `/api/tasks` can offer "attach". `fork_history` copies the parent's
/// transcript into it — true for `/btw` (a question about THIS
/// conversation), false for `delegate_task` (the contract is context-via-
/// argument; children know nothing of the parent otherwise).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_background(
    goal: String,
    ctx: String,
    model: Option<String>,
    provider: Option<String>,
    reasoning: Option<String>,
    parent: String,
    fork_history: bool,
    hooks: Option<std::sync::Arc<crate::hooks::HookBus>>,
) -> u32 {
    let (child_id, cancel) = crate::tasks::start(TaskKind::Subagent, &goal, &parent);
    // Mint + stamp SYNCHRONOUSLY, before returning the task id: the
    // caller's very next act is usually to tell the user "subagent N
    // started — /subagents to watch", and a listing that races the spawn
    // would show a subagent with no session. SQLite row insert is
    // microseconds; the fork copy is the only heavy part and it is still
    // far under a turn.
    let child_session = provision_child_session(&parent, &goal, fork_history);
    if let Some(cs) = &child_session {
        crate::tasks::set_child_session(child_id, cs);
    }
    let label = goal.clone();
    crate::bg::spawn(async move {
        if let Some(h) = &hooks {
            h.fire(
                "subagent_start",
                &crate::hooks::with_session(
                    &parent,
                    json!({"child_id": child_id, "goal": label, "child_session": child_session}),
                ),
            )
            .await;
        }
        let (g, c, p) = (goal.clone(), ctx, parent.clone());
        let cs_for_run = child_session.clone();
        let v = crate::rt::spawn_blocking(move || {
            run_child_routed(
                &g,
                &c,
                model.as_deref(),
                provider.as_deref(),
                reasoning.as_deref(),
                &p,
                cs_for_run.as_deref(),
                Some(cancel),
            )
        })
        .await
        .unwrap_or_else(|e| json!({"status": "error", "error": format!("join failed: {e}")}));
        let ok = v["status"].as_str() == Some("ok");
        crate::tasks::finish(child_id, ok);
        if let Some(h) = &hooks {
            h.fire(
                "subagent_stop",
                &crate::hooks::with_session(
                    &parent,
                    json!({"child_id": child_id, "goal": label, "ok": ok}),
                ),
            )
            .await;
            h.fire(
                "background_child_complete",
                &crate::hooks::with_session(
                    &parent,
                    json!({"child_id": child_id, "goal": label, "result": v}),
                ),
            )
            .await;
        }
        push_background_event(
            &parent,
            BackgroundEvent {
                child_id,
                kind: TaskKind::Subagent,
                goal,
                result: v,
                child_session: child_session.clone(),
                finished_at: std::time::SystemTime::now(),
            },
        );
        // The wake bell, same ORDER as the others (queue push first —
        // the wake loop reads `has_background_events` on receipt).
        crate::session_tap::publish(
            &parent,
            "child-complete",
            json!({
                "session_id": parent,
                "id": child_id,
                "goal": label,
                "ok": ok,
                "child_session": child_session,
            }),
        );
    });
    child_id
}

/// `/btw <prompt>` in the REPL, TUI and desktop: a one-off subagent
/// FORKED off this session — the child inherits the transcript (the
/// question is about this conversation), runs in its own titled child
/// session, and its answer returns here as a [BACKGROUND CHILD
/// COMPLETE] injection at the next turn boundary.
///
/// IT INHERITS THE ROUTE TOO, and that is the half that was missing.
/// USER 2026-09-18: *"/btw should inherit both session and
/// model/provider"* — a question about what you are doing, answered by
/// a different model than the one doing it, is a different answer. A
/// delegate is the opposite case and stays on the default; see the
/// tool description.
///
/// The route is read from the parent's `session_overrides` row rather
/// than from a live `Agent`, because /btw is called from four
/// frontends and only some of them hold one. An empty row means the
/// parent is itself on `[model] default`, and then so is the child.
pub fn spawn_background_child(goal: &str, parent_session: &str) -> u32 {
    let ov = parent_route(parent_session);
    spawn_background(
        goal.to_string(),
        String::new(),
        ov.model,
        ov.provider,
        ov.reasoning,
        parent_session.to_string(),
        true,
        crate::hooks::default_bus(),
    )
}

#[async_trait]
impl crate::tools::Tool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }
    fn description(&self) -> &str {
        "Spawn subagents in isolated one-shot sessions (`veles chat -q` children). \
         Use 'goal' for a single task or 'tasks' (≤3) for a parallel batch. Children \
         know nothing of this conversation — pass everything needed via 'context'. \
         Each child runs in its OWN real session (titled, parent-linked, attachable \
         from the session list — /subagents and /api/tasks name it), so its full \
         transcript is reviewable even after it reports back. \
         'agent': <name> runs a DEFINED subagent (from ./.veles/agents/*.md or \
         ~/.config/veles/agents/*.md — /agents lists them): its profile body becomes the \
         child's context and its model/provider/reasoning frontmatter routes the child. \
         Children CANNOT delegate further or ask questions. Child summaries are \
         SELF-REPORTS: verify external side effects (files, URLs, uploads) yourself \
         before reporting success. Children run on the DEFAULT route (`/defaults \
         model`), NOT on this session's — a delegate is not a continuation of this \
         conversation, and inheriting a rate-limited route would take a parallel \
         batch down with it. Pass 'model'/'provider'/'reasoning' to route one \
         explicitly, or 'agent' to run a defined subagent with its own routing."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "Single-task goal"},
                "context": {"type": "string", "description": "Background the child needs"},
                "agent": {"type": "string", "description": "Name of a defined subagent (.veles/agents/<name>.md); its profile + routing apply"},
                "model": {"type": "string", "description": "Route this child's model (default: [model] default from /defaults; an 'agent' preset's own routing wins)"},
                "provider": {"type": "string", "description": "Route this child's provider"},
                "reasoning": {"type": "string", "description": "Reasoning effort for this child"},
                "backend": {"type": "string", "description": "WHERE the child runs (compute pool): 'local', a node name/id from /nodes, 'gpu' (any live GPU node), or 'any' (least-loaded, self included). Default: this node. Honest error if unreachable."},
                "needs": {"type": "array", "items": {"type": "string"}, "description": "Programs the child's node must have, e.g. [\"rg\"] (/nodes lists them). Alone: any node with them."},
                "background": {"type": "boolean", "description": "Return immediately; the child's summary arrives as a [BACKGROUND CHILD COMPLETE] message when it finishes (use for long tasks you don't need to block on)"},
                "tasks": {
                    "type": "array",
                    "maxItems": MAX_BATCH,
                    "items": {
                        "type": "object",
                        "properties": {
                            "goal": {"type": "string"},
                            "context": {"type": "string"},
                            "backend": {"type": "string", "description": "Per-task WHERE (same spellings)"}
                        },
                        "required": ["goal"]
                    }
                }
            }
        })
    }
    async fn call(&self, args: Value) -> String {
        if child_env_depth() >= 1 {
            return "error: subagents cannot spawn further subagents (VELES_CHILD_DEPTH guard)"
                .into();
        }

        // ── B4 PLACEMENT ─────────────────────────────────────────────
        //
        // `backend` says WHERE the child runs. Parsed once here; every
        // mode (single, batch-per-task, background) resolves through
        // the same helper so the spellings cannot drift. A backend ask
        // inside a REMOTE child (depth ≥ 1, caught above) is already
        // refused — the wire variant of the same rule.
        //
        // Precedence: the call argument, then `[cluster]
        // delegate_backend` (the operator's default), then local.
        let backend_arg = match args["backend"].as_str().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => crate::config::load()
                .ok()
                .and_then(|c| c.cluster.delegate_backend)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_default(),
        };
        let backend = match crate::placement::BackendSpec::parse(&backend_arg) {
            Ok(b) => b,
            Err(e) => return format!("error: {e:#}"),
        };
        // `needs` (PLAN-phone-userland.md T2): programs the node that
        // runs the child must advertise (`veles nodes` lists them). With
        // no `backend`, needs alone means `any` node that has them, this
        // one included.
        let needs: Vec<String> = match &args["needs"] {
            Value::Null => Vec::new(),
            Value::Array(items) if items.iter().all(Value::is_string) => items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            other => {
                return format!(
                    "error: 'needs' is a list of program names like [\"rg\", \"podman\"], \
                     not {other}"
                )
            }
        };
        let backend = match backend {
            None if !needs.is_empty() => Some(crate::placement::BackendSpec::Any),
            b => b,
        };
        if backend.is_some() && args["agent"].as_str().is_some_and(|a| !a.trim().is_empty()) {
            return "error: 'agent' presets run with the parent's placement — \
                    combine 'backend' with an explicit model/provider instead"
                .into();
        }

        // THE AGENT PRESET, RESOLVED ONCE, FOR EVERY MODE.
        //
        // `agent` and `tasks` are sibling top-level properties in this
        // tool's own schema, so `{agent: "reviewer", tasks: [...]}` is a
        // legal call — and the key meant three different things
        // depending on which branch read it. Single mode resolved it
        // and refused an unknown name. Background mode resolved it with
        // `.and_then(agents::find)`, where None is indistinguishable
        // from "no preset asked for", so a typo ran the default route in
        // silence. Batch mode never read the key at all and passed
        // None/None/None to every child: a whole parallel batch on the
        // default route with no profile body, and a typo'd name spawning
        // children instead of saying so.
        //
        // One resolution, one refusal, before anything is spawned.
        let preset = match args["agent"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) => match crate::agents::find(name) {
                Some(a) => Some(a),
                None => {
                    return format!(
                        "error: no agent definition '{name}' — /agents lists the defined ones \
                         (./.veles/agents/*.md or {}/*.md)",
                        crate::config::config_dir().join("agents").display()
                    );
                }
            },
            None => None,
        };
        // THE EXPLICIT ROUTE, RESOLVED ONCE TOO, for the same reason.
        //
        // USER 2026-09-18: a delegate child runs on the default text
        // model from `/defaults` "unless explicitly overridden", and
        // until now the only way to override it was to define an
        // `agent:` preset — so a one-off "run this one on the big
        // model" meant writing a file. A preset still wins where both
        // are given: it is a named, reviewable definition whose
        // frontmatter IS its route, and silently overriding it from a
        // call argument would make `/agents` a lie about what ran.
        let explicit = |k: &str| {
            args[k]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let (ex_m, ex_p, ex_r) = (
            explicit("model"),
            explicit("provider"),
            explicit("reasoning"),
        );
        // Background mode (S3): spawn detached, return the child id.
        if args["background"].as_bool().unwrap_or(false) {
            if args["tasks"].is_array() {
                return "error: background mode takes a single 'goal', not 'tasks'".into();
            }
            let goal = args["goal"].as_str().unwrap_or_default().trim().to_string();
            if goal.is_empty() {
                return "error: 'goal' is required".into();
            }
            let ctx = args["context"].as_str().unwrap_or_default().to_string();
            let parent = self.session_id.clone();
            let (goal, ctx, model, provider, reasoning) = match &preset {
                Some(a) => {
                    let (p, m, pv, r) = crate::agents::render_child(a, &ctx, &goal);
                    (p, String::new(), m, pv, r)
                }
                None => (goal, ctx, ex_m.clone(), ex_p.clone(), ex_r.clone()),
            };
            // B4: a background placement runs the whole remote call
            // inside the detached task and delivers the peer's verdict
            // through the SAME background-event + wake path a local
            // background child uses — one arrival shape, local or
            // remote.
            let placement = match self.resolve_backend(backend.as_ref(), &needs) {
                Ok(p) => p,
                Err(e) => return format!("error: {e}"),
            };
            let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
                .ok()
                .flatten()
                .map(|i| i.id);
            let remote = placement
                .as_ref()
                .map(|p| !crate::placement::is_local(p, me.as_deref()))
                .unwrap_or(false);
            if remote {
                let Some(placement) = placement else {
                    unreachable!("remote implies Some")
                };
                let (child_id, cancel) = crate::tasks::start(TaskKind::Subagent, &goal, &parent);
                // No local session for remote work: the transcript
                // lives in the peer's store under the peer's own
                // minted id, which the verdict returns as
                // `remote_child_session`. Minting an empty local row
                // would be the 2026-09-18 debris bug wearing a
                // cluster hat. (goal here is the RAW goal: preset +
                // backend are refused together, so no preset
                // rendering has happened on this path.)
                let ask = self.remote_ask(
                    &goal,
                    &ctx,
                    model.clone(),
                    provider.clone(),
                    reasoning.clone(),
                );
                let goal2 = goal.clone();
                let bus = self.bus();
                let parent2 = parent.clone();
                let watch = crate::remotetask::CancelWatch::token(cancel);
                crate::bg::spawn(async move {
                    let v = run_on_placed(&placement, &ask, watch)
                        .await
                        .unwrap_or_else(|e| json!({"status": "error", "error": e}));
                    let ok = v["status"].as_str() == Some("ok");
                    // The transcript handle for remote work is the
                    // PEER's child session id (from its verdict) —
                    // there is no local row.
                    let remote_cs = v["remote_child_session"].as_str().map(|s| s.to_string());
                    crate::tasks::finish(child_id, ok);
                    if let Some(h) = &bus {
                        h.fire(
                            "background_child_complete",
                            &crate::hooks::with_session(
                                &parent2,
                                json!({"child_id": child_id, "goal": goal2, "result": v}),
                            ),
                        )
                        .await;
                    }
                    push_background_event(
                        &parent2,
                        BackgroundEvent {
                            child_id,
                            kind: TaskKind::Subagent,
                            goal: goal2,
                            result: v,
                            child_session: remote_cs,
                            finished_at: std::time::SystemTime::now(),
                        },
                    );
                    crate::session_tap::publish(
                        &parent2,
                        "child-complete",
                        json!({
                            "session_id": parent2,
                            "id": child_id,
                            "ok": ok,
                        }),
                    );
                });
                return json!({
                    "status": "started",
                    "child_id": child_id,
                    "note": "running on a REMOTE backend in the background; its summary will \
                             arrive as [BACKGROUND CHILD COMPLETE] in this conversation when done"
                })
                .to_string();
            }
            let child_id = spawn_background(
                goal,
                ctx,
                model,
                provider,
                reasoning,
                parent,
                false,
                self.bus(),
            );
            return json!({
                "status": "started",
                "child_id": child_id,
                "note": "running in the background; its summary will arrive as [BACKGROUND CHILD COMPLETE] in this conversation when done"
            })
            .to_string();
        }
        // Batch mode.
        if let Some(arr) = args["tasks"].as_array() {
            if arr.is_empty() {
                return "error: empty tasks array".into();
            }
            if arr.len() > MAX_BATCH {
                return format!("error: at most {MAX_BATCH} parallel tasks");
            }
            // B4: every member's placement is resolved BEFORE any of
            // them starts. The resolver is side-effect free, so an honest
            // error for task 3 leaves tasks 1 and 2 unstarted instead of
            // running with nobody left to collect their verdicts.
            let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
                .ok()
                .flatten()
                .map(|i| i.id);
            let mut planned = Vec::with_capacity(arr.len());
            for t in arr {
                let goal = t["goal"].as_str().unwrap_or_default().to_string();
                let ctx = t["context"].as_str().unwrap_or_default().to_string();
                if goal.trim().is_empty() {
                    return "error: every task needs a non-empty goal".into();
                }
                // A task may carry its own `backend`; an unset per-task
                // spelling inherits the top-level one.
                let task_backend_str = t["backend"]
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| backend_arg.clone());
                let task_spec = match crate::placement::BackendSpec::parse(&task_backend_str) {
                    Ok(None) if !needs.is_empty() => Some(crate::placement::BackendSpec::Any),
                    Ok(s) => s,
                    Err(e) => return format!("error: {e:#}"),
                };
                let placement = match self.resolve_backend(task_spec.as_ref(), &needs) {
                    Ok(p) => p.filter(|p| !crate::placement::is_local(p, me.as_deref())),
                    Err(e) => return format!("error: {e}"),
                };
                planned.push((goal, ctx, placement));
            }
            // Each member is STARTED here — a blocking child on the
            // blocking pool, a remote hop as a task — and only then
            // awaited. B4 had wrapped the synchronous `run_child_routed`
            // in a bare `async` block and awaited the futures one by one:
            // the batch ran its children in SERIES, each holding a
            // runtime worker for up to 900 s.
            let mut handles: Vec<tokio::task::JoinHandle<Value>> = Vec::new();
            for (goal, ctx, placement) in planned {
                let (child_id, cancel) =
                    crate::tasks::start(TaskKind::Subagent, &goal, &self.session_id);
                // Each LOCAL member is its own titled, parent-linked
                // session — same as a single delegate. No history fork:
                // the contract is context-via-argument. A remote member's
                // transcript lives in the peer's store under the peer's
                // id (B5 replicates it home); no local row means nothing
                // empty to dangle.
                let cs = if placement.is_some() {
                    None
                } else {
                    let cs = provision_child_session(&self.session_id, &goal, false);
                    if let Some(cs) = &cs {
                        crate::tasks::set_child_session(child_id, cs);
                    }
                    cs
                };
                // The DEFINITION was resolved once; the RENDERING is per
                // task, because render_child weaves this task's own goal
                // and context into the profile body.
                let (goal, ctx, model, provider, reasoning) = match &preset {
                    Some(a) => {
                        let (prompt, m, pv, r) = crate::agents::render_child(a, &ctx, &goal);
                        (prompt, String::new(), m, pv, r)
                    }
                    None => (goal, ctx, ex_m.clone(), ex_p.clone(), ex_r.clone()),
                };
                if let Some(p) = placement {
                    // C3: the registry token now rides the wire call —
                    // `veles tasks cancel <id>` reaches the peer's
                    // child instead of dying at the boundary.
                    let ask = self.remote_ask(&goal, &ctx, model, provider, reasoning);
                    let watch = crate::remotetask::CancelWatch::token(cancel);
                    handles.push(crate::rt::spawn(async move {
                        let v = run_on_placed(&p, &ask, watch)
                            .await
                            .unwrap_or_else(|e| json!({"status": "error", "error": e}));
                        crate::tasks::finish(child_id, v["status"].as_str() == Some("ok"));
                        v
                    }));
                } else {
                    let parent = self.session_id.clone();
                    handles.push(crate::rt::spawn_blocking(move || {
                        let v = run_child_routed(
                            &goal,
                            &ctx,
                            model.as_deref(),
                            provider.as_deref(),
                            reasoning.as_deref(),
                            &parent,
                            cs.as_deref(),
                            Some(cancel),
                        );
                        crate::tasks::finish(child_id, v["status"].as_str() == Some("ok"));
                        v
                    }));
                }
            }
            let mut results = Vec::new();
            for h in handles {
                results.push(h.await.unwrap_or_else(
                    |e| json!({"status": "error", "error": format!("child task join failed: {e}")}),
                ));
            }
            return json!({"batch": true, "results": results}).to_string();
        }
        // Single mode.
        let goal = args["goal"].as_str().unwrap_or_default().trim();
        if goal.is_empty() {
            return "error: 'goal' (or 'tasks') is required".into();
        }
        let ctx = args["context"].as_str().unwrap_or_default().to_string();
        let goal = goal.to_string();
        // B4: resolve the placement BEFORE minting any session. The
        // resolver is side-effect free (registry + config + identity
        // reads), so an honest error below leaves no debris — the
        // 2026-09-18 lesson is structural here, not a reap after the
        // fact.
        //
        // The explicit route is post-preset here (preset + backend was
        // refused above), so ex_m/ex_p/ex_r IS the child's route.
        match self
            .run_remote_if_placed(
                backend.as_ref(),
                &needs,
                &goal,
                &ctx,
                ex_m.as_deref(),
                ex_p.as_deref(),
                ex_r.as_deref(),
            )
            .await
        {
            Ok(None) => {}
            // A remote verdict carries the PEER's `remote_child_session`
            // as the transcript handle; the local store holds no row
            // for remote work (until B5 replicates transcripts, and
            // even then under the peer's id).
            Ok(Some(v)) => return v.to_string(),
            Err(e) => return format!("error: {e}"),
        }
        // The child session, minted against the RAW goal — after preset
        // rendering, `goal` is the full child prompt (profile body
        // included) and its first 60 chars are a useless title. Link
        // only, no history fork: the delegate contract is
        // context-via-argument, unlike /btw. Minted HERE, after the
        // remote branch returns: only a child that will run on THIS
        // node gets a local session row.
        let child_session = provision_child_session(&self.session_id, &goal, false);
        // S7: the agent preset resolved at the top of this call —
        // "agent": "name" loads .veles/agents/<name>.md, prepends its
        // body to the child context and applies its
        // model/provider/reasoning routing.
        let (goal, ctx, model, provider, reasoning) = match &preset {
            Some(a) => {
                let (prompt, m, p, r) = crate::agents::render_child(a, &ctx, &goal);
                (prompt, String::new(), m, p, r)
            }
            None => (goal, ctx, ex_m.clone(), ex_p.clone(), ex_r.clone()),
        };
        let (child_id, cancel) = crate::tasks::start(TaskKind::Subagent, &goal, &self.session_id);
        if let Some(cs) = &child_session {
            crate::tasks::set_child_session(child_id, cs);
        }
        if let Some(h) = &self.bus() {
            h.fire(
                "subagent_start",
                &crate::hooks::with_session(
                    &self.session_id,
                    json!({"child_id": child_id, "goal": goal, "child_session": child_session}),
                ),
            )
            .await;
        }
        let label = goal.clone();
        let parent = self.session_id.clone();
        let cs_for_run = child_session.clone();
        let v = crate::rt::spawn_blocking(move || {
            let v = run_child_routed(
                &goal,
                &ctx,
                model.as_deref(),
                provider.as_deref(),
                reasoning.as_deref(),
                &parent,
                cs_for_run.as_deref(),
                Some(cancel),
            );
            let ok = v["status"].as_str() == Some("ok");
            crate::tasks::finish(child_id, ok);
            v
        })
        .await
        .unwrap_or_else(|e| json!({"status": "error", "error": format!("join failed: {e}")}));
        if let Some(h) = &self.bus() {
            h.fire(
                "subagent_stop",
                &crate::hooks::with_session(
                    &self.session_id,
                    json!({"child_id": child_id, "goal": label, "ok": v["status"] == "ok"}),
                ),
            )
            .await;
        }
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    fn completion(id: u32) -> BackgroundEvent {
        BackgroundEvent {
            child_id: id,
            kind: TaskKind::Monitor,
            goal: "exit 0".into(),
            result: json!({"summary": "condition met"}),
            child_session: None,
            finished_at: std::time::SystemTime::now(),
        }
    }

    /// One completion has two representations of the SAME queue entry:
    /// its durable crash-recovery row and its in-process fast-path copy.
    /// Draining both without reconciling them sends the banner twice to
    /// the model and persists it twice in the transcript. Observed on a
    /// real isolated gateway with a monitor waking an idle desktop turn.
    #[test]
    fn a_durable_completion_and_its_memory_copy_drain_exactly_once() {
        let sid = format!("bgevent-exactly-once-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        store.lock_ok().create_session(&sid).unwrap();
        push_background_event_to(&sid, completion(41), Some(store.clone()));
        let drained = take_background_events_from(&sid, Some(store.clone()));
        assert_eq!(drained.len(), 1, "one completion must drain once");
        assert_eq!(drained[0].child_id, 41);
        assert!(take_background_events_from(&sid, Some(store.clone())).is_empty());
        assert!(store.lock_ok().queue_items(&sid).unwrap().is_empty());
        let _ = take_background_events(&sid);
    }

    /// A row from a process that has already exited has no memory copy.
    /// It still drains, once, when the next process takes its events.
    #[test]
    fn a_durable_completion_without_a_memory_copy_still_drains_once() {
        let sid = format!("bgevent-recovery-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        store.lock_ok().create_session(&sid).unwrap();
        let body = event_to_body(&completion(42)).unwrap();
        store
            .lock_ok()
            .queue_push_kind(&sid, &body, "bgevent")
            .unwrap();
        let drained = take_background_events_from(&sid, Some(store.clone()));
        assert_eq!(drained.len(), 1, "the restart-recovery row must survive");
        assert_eq!(drained[0].child_id, 42);
        assert!(take_background_events_from(&sid, Some(store)).is_empty());
    }

    /// `shell wait` consumed the shell's output already. Its completion
    /// must disappear from BOTH docks; a monitor with the same numeric
    /// task id is unrelated work and must remain queued.
    #[test]
    fn discarding_a_shell_completion_clears_both_docks_only_for_that_kind() {
        let sid = format!("bgevent-discard-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        store.lock_ok().create_session(&sid).unwrap();
        let mut shell = completion(50);
        shell.kind = TaskKind::Shell;
        shell.goal = "long build".into();
        push_background_event_to(&sid, shell, Some(store.clone()));
        push_background_event_to(&sid, completion(50), Some(store.clone()));
        assert!(discard_shell_completion_from(&sid, 50, Some(store.clone())));
        let rest = take_background_events_from(&sid, Some(store));
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].kind, TaskKind::Monitor);
        assert_eq!(rest[0].child_id, 50);
    }

    #[test]
    fn background_events_roundtrip_and_drain() {
        // S3: a completion pushed for one session is taken (drained) by
        // that session only, in order; other sessions see nothing.
        push_background_event(
            "sess-a",
            BackgroundEvent {
                child_id: 1,
                kind: TaskKind::Subagent,
                goal: "check the build".into(),
                result: json!({"status": "ok", "summary": "build passed"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        push_background_event(
            "sess-a",
            BackgroundEvent {
                child_id: 2,
                kind: TaskKind::Subagent,
                goal: "lint".into(),
                result: json!({"status": "ok", "summary": "lint clean"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        push_background_event(
            "sess-b",
            BackgroundEvent {
                child_id: 3,
                kind: TaskKind::Subagent,
                goal: "other".into(),
                result: json!({"status": "error", "error": "boom"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );

        // A sees both, in FIFO order; B untouched.
        let a = take_background_events("sess-a");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].child_id, 1);
        assert_eq!(a[1].child_id, 2);
        // Draining is destructive: second take is empty.
        assert!(take_background_events("sess-a").is_empty());
        // B still has its event.
        let b = take_background_events("sess-b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].child_id, 3);
        // A session with nothing gets an empty vec.
        assert!(take_background_events("sess-none").is_empty());
    }

    /// The idle-wake claim: two wake owners (gateway + desktop) both
    /// subscribe to the same session bus, so one completion event could
    /// start two turns on one session. The claim is the process-wide
    /// one-at-a-time gate — the first waker holds it for the whole
    /// turn, the second stands down — and dropping the claim frees the
    /// next completion to wake the session again (a wake turn that
    /// panics must not leave the session unwakable forever).
    #[test]
    fn the_wake_claim_admits_one_waker_and_releases_on_drop() {
        // Leave no claim behind from an earlier run: the map is
        // process-global and a forgotten guard would make this test's
        // first claim look like a second.
        let _ = wake_claims().lock_ok().remove("wake-claim-sess");
        let first = try_claim_wake("wake-claim-sess").expect("first waker claims");
        assert!(
            try_claim_wake("wake-claim-sess").is_none(),
            "a held claim must refuse the second waker"
        );
        drop(first);
        let again =
            try_claim_wake("wake-claim-sess").expect("a released claim must admit the next waker");
        drop(again);
    }

    #[test]
    fn a_released_wake_claim_rings_for_a_completion_that_arrived_during_the_wake() {
        let sid = format!("wake-claim-pending-{}", uuid::Uuid::new_v4());
        let _ = wake_claims().lock_ok().remove(&sid);
        let mut rx = crate::session_tap::subscribe();
        let claim = try_claim_wake(&sid).expect("first wake claims");
        let mut claim = claim;
        claim.retry_pending_on_drop();
        push_background_event(
            &sid,
            BackgroundEvent {
                child_id: 78,
                kind: TaskKind::Shell,
                goal: "finished during the first wake's final response".into(),
                result: json!({"summary": "second result"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        drop(claim);
        let mut found = false;
        while let Ok(ev) = rx.try_recv() {
            if ev.session_id == sid && ev.kind == "background-wake-pending" {
                found = true;
            }
        }
        assert!(found, "the second completion was stranded by the old claim");
        let retry = try_claim_wake(&sid).expect("the retry can claim afresh");
        drop(retry);
        let _ = take_background_events(&sid);
    }

    #[test]
    fn an_unadmitted_wake_claim_does_not_spin_on_parked_events() {
        let sid = format!("wake-claim-muted-{}", uuid::Uuid::new_v4());
        let _ = wake_claims().lock_ok().remove(&sid);
        let mut rx = crate::session_tap::subscribe();
        let claim = try_claim_wake(&sid).expect("claim before admission");
        push_background_event(
            &sid,
            BackgroundEvent {
                child_id: 79,
                kind: TaskKind::Shell,
                goal: "a deliberately muted completion".into(),
                result: json!({"summary": "wait for a real turn"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        drop(claim);
        while let Ok(ev) = rx.try_recv() {
            assert!(
                !(ev.session_id == sid && ev.kind == "background-wake-pending"),
                "an unadmitted wake started a retry loop: {ev:?}"
            );
        }
        let _ = take_background_events(&sid);
    }

    /// Stage 3, the heart of "children are real sessions": provisioning
    /// mints a titled, parent-linked child row BEFORE the child process
    /// exists, so the session list can show it and the user can attach.
    /// A `/btw` fork also copies the parent transcript (the question is
    /// about THIS conversation); a `delegate_task` child does NOT (the
    /// contract is context-via-argument). Both branches assert against
    /// the same scratch store the production path opens via
    /// `veles_home()`, pointed at a tempdir by the thread-scoped
    /// `DirOverride` guard.
    #[test]
    fn provisioning_makes_a_titled_forked_child_session() {
        let dir = tempfile::tempdir().unwrap();
        let _dirs = crate::config::set_dirs(Some(crate::config::DirOverride::forced(dir.path())));
        // Seed the parent with a transcript through the SAME store the
        // provisioner opens (veles_home()/state.db).
        let store = crate::store::Store::open(&crate::config::veles_home().join("state.db"))
            .expect("scratch store");
        store.create_session("parent-sess").unwrap();
        store
            .append_message(
                "parent-sess",
                &crate::providers::ChatMessage::user("earlier talk"),
            )
            .unwrap();
        store.set_title("parent-sess", "the parent").unwrap();
        drop(store);

        // FORK (the /btw path): parent link + title + copied history.
        let forked = provision_child_session("parent-sess", "what did we decide?", true)
            .expect("provisioned a child");
        let store = crate::store::Store::open(&crate::config::veles_home().join("state.db"))
            .expect("scratch store");
        assert_eq!(
            store.session_parent(&forked).unwrap().as_deref(),
            Some("parent-sess"),
            "the forked child records its parent"
        );
        let title = store.title(&forked).unwrap().unwrap_or_default();
        assert!(
            title.starts_with("subagent:"),
            "the child is titled for the session list: {title}"
        );
        let hist = store.load_history(&forked).unwrap();
        assert!(
            hist.iter().any(|m| m.content == "earlier talk"),
            "a fork inherits the parent transcript so the child can answer about it"
        );
        drop(store);

        // LINK ONLY (the delegate_task path): parent link + title, but
        // NO copied history — the child starts clean and is told
        // everything through its prompt.
        let linked = provision_child_session("parent-sess", "build the thing", false)
            .expect("provisioned a child");
        let store = crate::store::Store::open(&crate::config::veles_home().join("state.db"))
            .expect("scratch store");
        assert_eq!(
            store.session_parent(&linked).unwrap().as_deref(),
            Some("parent-sess"),
            "a delegate child is still linked to its parent"
        );
        let hist2 = store.load_history(&linked).unwrap();
        assert!(
            !hist2.iter().any(|m| m.content == "earlier talk"),
            "a delegate child does NOT inherit the transcript: context-via-argument"
        );
        drop(store);
    }

    #[test]
    fn the_wake_peek_is_nondestructive() {
        // The waker stands down silently when there is nothing to run
        // — but only AFTER admitting; a peek that eats the events
        // would starve the turn that actually gets to drain.
        assert!(!has_background_events("wake-peek-none"));
        push_background_event(
            "wake-peek-sess",
            BackgroundEvent {
                child_id: 7,
                kind: TaskKind::Shell,
                goal: "x".into(),
                result: json!({"summary": "x"}),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        assert!(has_background_events("wake-peek-sess"));
        // The peek did not consume: the events are still there.
        assert_eq!(take_background_events("wake-peek-sess").len(), 1);
    }

    /// `agent:` is a top-level property beside `tasks:` in this tool's
    /// own schema, so `delegate_task {agent: "x", tasks: [...]}` is a
    /// legal call. Single mode resolved the preset and REFUSED a name
    /// that does not exist; background mode was fixed to resolve it too
    /// (its comment records that it "used to call bare run_child,
    /// silently dropping the agent preset ... only single mode honoured
    /// them") but still turned an unknown name into a silent None.
    /// Batch mode was the third case and had neither: it never read
    /// `agent` at all and passed None/None/None to every child, so a
    /// whole parallel batch ran on the default route with no profile
    /// body — and a TYPO'd name spawned children instead of saying so.
    ///
    /// The refusal must arrive BEFORE anything is spawned, which is why
    /// this test needs no binary at all.
    ///
    /// scratch_home(), not lock(): it pins VELES_HOME and restores the
    /// cwd, so neither the real state.db nor the process's working
    /// directory is touched. Sync, so the env guard is never held
    /// across an await (clippy::await_holding_lock).
    #[test]
    fn a_batch_refuses_an_unknown_agent_like_every_other_mode() {
        let (_home, _env) = crate::testenv::scratch_home();
        let t = DelegateTaskTool {
            session_id: "batch-unknown-agent".into(),
            hooks: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(t.call(json!({
            "agent": "no-such-reviewer",
            "tasks": [{"goal": "one"}, {"goal": "two"}]
        })));
        assert!(
            out.contains("no-such-reviewer") && out.contains("/agents"),
            "the batch must refuse by name and point at the listing, got: {out}"
        );
        assert!(
            !out.contains("\"batch\""),
            "and must not have run a batch at all: {out}"
        );
    }

    /// The other direction: a KNOWN preset must reach EVERY child of the
    /// batch — its profile body as context, its frontmatter as the
    /// child's route. Observed at the spawn: VELES_BINARY points at a
    /// script that records its own argv, so this asserts on what was
    /// really handed to the child processes rather than on an internal.
    #[cfg(unix)]
    #[test]
    fn a_batch_applies_the_agent_presets_profile_and_routing() {
        let (home, _env) = crate::testenv::scratch_home();
        // Under VELES_HOME/agents, not ./.veles/agents: agents::roots()
        // reads both, and the home one needs no chdir — which is the
        // process-global whose restore this suite has been bitten by.
        std::fs::create_dir_all(home.path().join("agents")).unwrap();
        std::fs::write(
            home.path().join("agents/reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews\nmodel: preset-model-9\n---\n\nYOU ARE THE REVIEWER: check layering first.\n",
        )
        .unwrap();

        // A "veles" that records what it was asked to do and exits 0.
        let log = home.path().join("argv.log");
        let fake = home.path().join("fake-veles");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> {}\necho done\n",
                log.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);

        let t = DelegateTaskTool {
            session_id: "batch-preset".into(),
            hooks: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(t.call(json!({
            "agent": "reviewer",
            "tasks": [{"goal": "review the parser"}, {"goal": "review the store"}]
        })));
        std::env::remove_var("VELES_BINARY");
        assert!(out.contains("\"batch\""), "the batch ran: {out}");

        let argv = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            argv.matches("YOU ARE THE REVIEWER").count(),
            2,
            "BOTH children carry the preset's profile body, not just the first: {argv}"
        );
        assert_eq!(
            argv.matches("preset-model-9").count(),
            2,
            "and both carry its routing: {argv}"
        );
        assert!(
            argv.contains("review the parser") && argv.contains("review the store"),
            "each child still gets its OWN goal: {argv}"
        );
    }

    /// A batch runs its children IN PARALLEL. B4 wrapped the blocking
    /// child runner in a bare `async` block and awaited the futures one
    /// by one, so three children ran in series — each also pinning a
    /// runtime worker for up to 900 s.
    ///
    /// Asserted as OVERLAP, not as a wall-clock total: each fake child
    /// records when it started and ended, and every start must come
    /// before the first end — true of a parallel batch however slow the
    /// machine, false of a serial one however fast. The first version
    /// required three 1 s children to finish within 2.5 s, and the loaded
    /// nix sandbox took 2.57 s for a batch that did run in parallel.
    #[cfg(unix)]
    #[test]
    fn a_batch_runs_its_children_in_parallel() {
        let (home, _env) = crate::testenv::scratch_home();
        let fake = home.path().join("fake-veles");
        let spans = home.path().join("spans");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\ns=$(date +%s%N)\nsleep 2\necho \"$s $(date +%s%N)\" >> '{}'\necho done\n",
                spans.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);
        let t = DelegateTaskTool {
            session_id: "batch-parallel".into(),
            hooks: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(t.call(json!({
            "tasks": [{"goal": "one"}, {"goal": "two"}, {"goal": "three"}]
        })));
        std::env::remove_var("VELES_BINARY");
        assert_eq!(
            out.matches("\"status\":\"ok\"").count(),
            3,
            "all three ran: {out}"
        );
        let spans: Vec<(u128, u128)> = std::fs::read_to_string(&spans)
            .expect("each child records its span")
            .lines()
            .map(|l| {
                let (a, b) = l.split_once(' ').expect("start end");
                (a.parse().unwrap(), b.parse().unwrap())
            })
            .collect();
        assert_eq!(spans.len(), 3, "{spans:?}");
        let last_start = spans.iter().map(|s| s.0).max().unwrap();
        let first_end = spans.iter().map(|s| s.1).min().unwrap();
        assert!(
            last_start < first_end,
            "a child started after another had finished — they ran one after another: {spans:?}"
        );
    }

    /// The ask a remote child starts from carries the CALLER's principal
    /// — the placing session's, serialized like a local child's env —
    /// and none for an owner session.
    #[test]
    fn a_remote_ask_carries_the_callers_principal() {
        let _g = crate::testenv::lock();
        let sess = format!("remote-ask-principal-{}", std::process::id());
        crate::acl::set_session_principal(
            &sess,
            crate::acl::Principal {
                id: Some("irc:someone".into()),
                subject: "user:someone".into(),
                role: crate::acl::Role::Trusted,
                caps: crate::acl::Role::Trusted.default_caps(),
                rule_context_limit: None,
                mask: None,
                via: Some("test".into()),
                ticket_role: None,
            },
        );
        let t = DelegateTaskTool {
            session_id: sess,
            hooks: None,
        };
        let ask = t.remote_ask("g", "", None, None, None);
        let carried = ask.principal.expect("a Trusted caller's principal travels");
        let back: crate::acl::Principal = serde_json::from_str(&carried).unwrap();
        assert_eq!(back.subject, "user:someone");
        assert_eq!(back.role, crate::acl::Role::Trusted);

        let owner = DelegateTaskTool {
            session_id: "remote-ask-owner-no-principal".into(),
            hooks: None,
        };
        assert!(owner
            .remote_ask("g", "", None, None, None)
            .principal
            .is_none());
    }

    #[test]
    fn veles_binary_override() {
        let _g = crate::testenv::lock();
        std::env::set_var("VELES_BINARY", "/nonexistent/veles");
        assert_eq!(veles_binary().unwrap(), PathBuf::from("/nonexistent/veles"));
        std::env::remove_var("VELES_BINARY");
    }

    #[cfg(unix)]
    #[test]
    fn a_delegate_child_carries_the_callers_principal_not_the_owner_surface() {
        // The hole this pins: `veles chat` with no principal IS the
        // owner surface, so a delegate child used to inherit owner
        // rights from a trusted+ gate. The parent now serializes the
        // CALLER's principal into the child's environment (and the chat
        // boot path re-binds it). This runs the REAL spawn path against
        // a fake veles binary that echoes the env it was handed — if
        // the env line disappears, the child is an owner again and this
        // test is the one that says so.
        let _g = crate::testenv::lock();
        let dir = std::env::temp_dir().join(format!(
            "veles-delegate-principal-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("veles");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'PRINCIPAL=[%s]\\n' \"$VELES_SESSION_PRINCIPAL\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&script).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&script, perm).unwrap();
        }
        std::env::set_var("VELES_BINARY", &script);

        let sess = format!("delegate-principal-{}", std::process::id());
        crate::acl::set_session_principal(
            &sess,
            crate::acl::Principal {
                id: Some("irc:someone".into()),
                subject: "user:someone".into(),
                role: crate::acl::Role::Trusted,
                caps: crate::acl::Role::Trusted.default_caps(),
                rule_context_limit: None,
                mask: Some("someone!u@h".into()),
                via: Some("test".into()),
                ticket_role: None,
            },
        );

        let v = run_child_routed("echo the env", "", None, None, None, &sess, None, None);
        let s = v.to_string();
        assert!(
            s.contains("PRINCIPAL=[{"),
            "child did not receive the principal env: {s}"
        );
        // The summary is embedded (and quote-escaped) inside the result
        // Value, so assert on the subject itself, not on the JSON shape.
        assert!(
            s.contains("user:someone"),
            "env did not carry the caller's subject: {s}"
        );

        // And the negative space: a parent with NO principal (an owner
        // CLI turn) sets no var — the child stays the owner's own
        // surface, which is correct because the caller was the owner.
        let v2 = run_child("echo again", "", None);
        let s2 = v2.to_string();
        assert!(
            s2.contains("PRINCIPAL=[]"),
            "a parent without a principal must not mint one: {s2}"
        );

        std::env::remove_var("VELES_BINARY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A spawn that FAILS must not leave a titled subagent session
    /// behind.
    ///
    /// `provision_child_session` mints the row before the process
    /// exists, deliberately — /subagents has to be able to offer
    /// "attach" the moment the task exists. When the spawn then dies
    /// the row stays, and on 2026-09-18 three of them were sitting in
    /// the user's session list, one per delegate_task call that died in
    /// clap: titled `subagent: …`, parent-linked, zero messages,
    /// created_at == updated_at.
    ///
    /// The NEGATIVE half is the one that matters: a child that ran and
    /// then failed keeps its transcript. Reaping by "the spawn failed"
    /// rather than by "the session is empty" would delete real work.
    #[cfg(unix)]
    #[test]
    fn a_child_that_never_started_leaves_no_empty_session_behind() {
        // NO `testenv::lock()` here: `scratch_home` takes the same
        // non-reentrant mutex through `guard()`, so holding it first
        // deadlocks the test. The EnvGuard it returns IS the lock.
        let (home, _env) = crate::testenv::scratch_home();

        // A "veles" that exits 2 writing nothing — what a clap usage
        // error looks like from the parent's side.
        let fake = home.path().join("fake-veles");
        std::fs::write(&fake, "#!/bin/sh\nexit 2\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);

        let db = crate::config::veles_home().join("state.db");
        let store = crate::store::Store::open(&db).expect("scratch store");
        store.create_session("reap-parent").unwrap();
        drop(store);

        // (1) EMPTY child, failed spawn → the row goes.
        let empty = provision_child_session("reap-parent", "do a thing", false)
            .expect("provisioned a child");
        let v = run_child_routed(
            "do a thing",
            "",
            None,
            None,
            None,
            "reap-parent",
            Some(&empty),
            None,
        );
        assert_eq!(v["status"], "error", "the spawn failed: {v}");
        let store = crate::store::Store::open(&db).expect("scratch store");
        assert!(
            !store.session_exists(&empty).unwrap(),
            "an empty child session left behind by a failed spawn is debris"
        );

        // (2) A child that SPOKE and then failed keeps its transcript.
        let spoke = provision_child_session("reap-parent", "do another thing", false)
            .expect("provisioned a child");
        store
            .append_message(&spoke, &crate::providers::ChatMessage::user("it ran"))
            .unwrap();
        drop(store);
        let v = run_child_routed(
            "do another thing",
            "",
            None,
            None,
            None,
            "reap-parent",
            Some(&spoke),
            None,
        );
        assert_eq!(v["status"], "error", "this spawn failed too: {v}");
        let store = crate::store::Store::open(&db).expect("scratch store");
        assert!(
            store.session_exists(&spoke).unwrap(),
            "a child that ran and THEN failed keeps its session — the \
             transcript is the whole reason the session is minted early"
        );
        drop(store);

        std::env::remove_var("VELES_BINARY");
    }

    /// A delegate child takes the DEFAULT route; a /btw child takes the
    /// parent's. Both halves in one test, because a pin that only
    /// checks the negative is satisfied by a child that gets no route
    /// at all.
    ///
    /// USER 2026-09-18: "for delegates it should use the default text
    /// model from /defaults, unless explicitly overridden" … "/btw
    /// should inherit both session and model/provider". The motivation
    /// is concrete: deepseek's RPM limit blows up with more than about
    /// two concurrent sessions, so a batch of children inheriting a
    /// rate-limited parent is three dead children.
    ///
    /// Observed at the SPAWN — VELES_BINARY points at a script that
    /// records its own argv — so this asserts on what the child
    /// process was really handed.
    #[cfg(unix)]
    #[test]
    fn a_delegate_takes_the_default_route_and_btw_takes_the_parents() {
        // No testenv::lock(): scratch_home takes the same mutex.
        let (home, _env) = crate::testenv::scratch_home();
        let log = home.path().join("argv.log");
        let fake = home.path().join("fake-veles");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> {}\necho done\n",
                log.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);

        // A parent pinned OFF the default with /model.
        let db = crate::config::veles_home().join("state.db");
        let store = crate::store::Store::open(&db).expect("scratch store");
        store.create_session("route-parent").unwrap();
        store
            .override_replace(
                "route-parent",
                Some("parent-prov"),
                Some("parent-model-7"),
                Some("high"),
            )
            .unwrap();
        drop(store);

        // The route resolver reads exactly that row.
        let ov = parent_route("route-parent");
        assert_eq!(ov.model.as_deref(), Some("parent-model-7"));
        assert_eq!(ov.provider.as_deref(), Some("parent-prov"));
        assert_eq!(ov.reasoning.as_deref(), Some("high"));
        // …and a session with no row is not pinned to anything, which
        // is what "fall through to [model] default" has to look like.
        assert!(parent_route("no-such-session").model.is_none());

        // (1) DELEGATE: the parent's route must not reach the child.
        let t = DelegateTaskTool {
            session_id: "route-parent".into(),
            hooks: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rt.block_on(t.call(json!({"goal": "audit the loader"})));
        let argv = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !argv.contains("parent-model-7") && !argv.contains("parent-prov"),
            "a delegate child inherited the parent's route: {argv}"
        );
        assert!(
            argv.contains("audit the loader"),
            "…and it did run, so the assertion above is about the route \
             and not about a child that never spawned: {argv}"
        );

        // (2) EXPLICIT override: "unless explicitly overridden".
        std::fs::write(&log, "").unwrap();
        let _ = rt.block_on(t.call(json!({
            "goal": "audit the store",
            "model": "asked-for-9",
            "provider": "asked-for-prov",
            "reasoning": "low",
        })));
        let argv = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            argv.contains("asked-for-9") && argv.contains("asked-for-prov") && argv.contains("low"),
            "an explicit per-child route must reach the child: {argv}"
        );

        // (3) /btw: the parent's route, both halves of it.
        std::fs::write(&log, "").unwrap();
        let _ = spawn_background_child("what did we decide?", "route-parent");
        // The child is detached on the bg runtime; the fake exits at
        // once, so a bounded poll is enough and cannot wedge the gate.
        let mut argv = String::new();
        for _ in 0..200 {
            argv = std::fs::read_to_string(&log).unwrap_or_default();
            if argv.contains("-Q") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            argv.contains("parent-model-7") && argv.contains("parent-prov"),
            "a /btw child must run on the session it forked off: {argv}"
        );

        std::env::remove_var("VELES_BINARY");
    }

    /// The description is the only thing the model reads before
    /// choosing a route, and it said the opposite of the code: "per-
    /// child model routing is not exposed here". It was not exposed,
    /// and the sentence stayed true by the tool being less useful than
    /// it should be; now it is exposed and the sentence would be a
    /// lie. A description that describes the opposite of the code is a
    /// defect, and the only thing that can hold it is a test.
    #[test]
    fn the_description_says_which_route_a_child_takes() {
        let d = DelegateTaskTool {
            session_id: "s".into(),
            hooks: None,
        };
        let text = crate::tools::Tool::description(&d);
        assert!(
            !text.contains("is not exposed here"),
            "the description still refuses a knob the schema now has: {text}"
        );
        assert!(
            text.contains("/defaults"),
            "it must name where a child's default route comes from: {text}"
        );
        let schema = crate::tools::Tool::schema(&d);
        for k in ["model", "provider", "reasoning"] {
            assert!(
                schema["properties"][k].is_object(),
                "the description promises '{k}' — the schema must offer it"
            );
        }
    }

    /// A child's events go to a bus inside the CHILD's process, which
    /// has no subscribers — so a window attached to a running child
    /// gets the store replay and then silence (USER 2026-09-18, "it
    /// stopped streaming and looked hung"). The child only learns
    /// where to send them from the environment the parent hands it.
    ///
    /// Asserted against the ENV the spawn really set, via a fake that
    /// records it — the same shape as the argv tests, and for the same
    /// reason: a builder pinned by reading itself proves nothing.
    #[cfg(unix)]
    #[test]
    fn a_spawned_child_is_told_where_to_relay_its_events() {
        // No testenv::lock(): scratch_home takes the same mutex.
        let (home, _env) = crate::testenv::scratch_home();
        let log = home.path().join("env.log");
        let fake = home.path().join("fake-veles");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf 'RELAY=%s\\n' \"$VELES_RELAY_TO\" >> {}\n\
                 printf 'DEPTH=%s\\n' \"$VELES_CHILD_DEPTH\" >> {}\necho done\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);
        // Deliberately NOT set here. `Command` inherits this process's
        // environment, so a test that exports it would pass whether or
        // not the spawn set anything — which is exactly what the first
        // version of this test did, and it survived deleting the code
        // it was meant to pin. The parent has to COMPUTE the target.
        std::env::remove_var(crate::relay::RELAY_ENV);

        let _ = run_child_routed("goal", "", None, None, None, "relay-parent", None, None);

        std::env::remove_var("VELES_BINARY");
        let recorded = std::fs::read_to_string(&log).unwrap_or_default();
        let port = crate::config::desktop_port(&crate::config::live());
        assert!(
            recorded.contains(&format!("RELAY=http://127.0.0.1:{port}")),
            "the child must be told where to post its events, at the port \
             the gateway actually binds: {recorded:?}"
        );
        assert!(
            recorded.contains("DEPTH=1"),
            "…without disturbing the depth guard it already carried: {recorded:?}"
        );
    }

    // ── B4: the delegate_task `backend` argument ───────────────────

    /// The tool must be constructed against a live registry: scratch
    /// home, an identity, and a peer node row with NO heartbeat-fresh
    /// problems. Returns (home-tempdir, env-guard, tool) — the CALLER
    /// must hold the tempdir: `scratch_home`'s guard pins the
    /// directory OVERRIDE, and letting the TempDir drop here deletes
    /// the very tree the guard still points at (state.db and node.toml
    /// included), which made every resolve below read an empty
    /// registry. The registry node has no `[a2a_agents]` entry unless
    /// the test writes one, which is how the honest-error arms are
    /// reached.
    fn backend_tool(
        peer_name: &str,
        peer_id: &str,
    ) -> (
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        DelegateTaskTool,
    ) {
        let (home, env) = crate::testenv::scratch_home();
        // This node's identity, so `local`/`any` can compare against it.
        std::fs::write(
            home.path().join("node.toml"),
            "id = \"self-0001\"\nname = \"laptop\"\n",
        )
        .unwrap();
        let store =
            crate::store::Store::open(&crate::config::veles_home().join("state.db")).unwrap();
        store
            .node_register(
                "self-0001",
                "laptop",
                "h",
                "0",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .node_register(
                peer_id,
                peer_name,
                "h",
                "0",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        // A fresh heartbeat so the peer is not stale.
        store.node_heartbeat(peer_id, Some(5)).unwrap();
        drop(store);
        let t = DelegateTaskTool {
            session_id: "backend-parent".into(),
            hooks: None,
        };
        (home, env, t)
    }

    /// A named backend that resolves to a live node with no dialing
    /// route ([a2a_agents] entry) is an HONEST ERROR, never a silent
    /// local run: asking for a node is a placement constraint, and
    /// quietly doing the work here is the failure the constraint
    /// exists to prevent.
    #[tokio::test]
    async fn a_named_backend_with_no_route_is_an_honest_error() {
        let (_home, _env, t) = backend_tool("vps", "peer-0002");
        let out = t
            .call(json!({"goal": "do the work", "backend": "vps"}))
            .await;
        assert!(
            out.contains("a2a_agents") && out.starts_with("error:"),
            "names the missing route and does not run locally: {out}"
        );
        // It did NOT fall through to a local spawn (no VELES_BINARY
        // set, so a local attempt would be a different error).
        assert!(!out.contains("ran locally"), "{out}");
    }

    /// `needs` reaches placement (PLAN-phone-userland.md T2): a named
    /// node that does not advertise a needed program is an error naming
    /// it, `needs` alone means `any` (and nobody here advertises
    /// anything, so it says so), and a malformed `needs` is refused —
    /// never a child spawned somewhere that cannot do the work.
    #[tokio::test]
    async fn delegate_needs_reach_placement() {
        let (_home, _env, t) = backend_tool("vps", "peer-0002");
        let out = t
            .call(json!({"goal": "g", "backend": "vps", "needs": ["podman"]}))
            .await;
        assert!(
            out.starts_with("error:") && out.contains("does not have podman"),
            "{out}"
        );
        let out = t.call(json!({"goal": "g", "needs": ["podman"]})).await;
        assert!(
            out.starts_with("error:") && out.contains("needs = [podman]"),
            "{out}"
        );
        let out = t.call(json!({"goal": "g", "needs": "podman"})).await;
        assert!(out.starts_with("error: 'needs'"), "{out}");
    }

    /// An unknown backend name is an error, not a default — a typo'd
    /// `vps2` must not quietly mean "any".
    #[tokio::test]
    async fn an_unknown_backend_is_an_error_not_a_default() {
        let (_home, _env, t) = backend_tool("vps", "peer-0002");
        let out = t
            .call(json!({"goal": "do the work", "backend": "vps2"}))
            .await;
        assert!(
            out.starts_with("error:") && out.contains("no such node"),
            "{out}"
        );
    }

    /// A preset and an explicit backend are mutually exclusive: a
    /// preset runs with the parent's placement by definition, so
    /// combining them is refused rather than one silently winning.
    #[tokio::test]
    async fn an_agent_preset_and_a_backend_are_refused_together() {
        let (_home, _env, t) = backend_tool("vps", "peer-0002");
        let out = t
            .call(json!({"goal": "g", "agent": "reviewer", "backend": "vps"}))
            .await;
        assert!(
            out.starts_with("error:") && out.contains("placement"),
            "{out}"
        );
    }

    /// The config default `[cluster] delegate_backend` applies when the
    /// call omits `backend` — the operator's placement policy, not the
    /// model's per-call choice. Same honest-error surface as an
    /// explicit ask, proving the default is really consulted.
    #[tokio::test]
    async fn a_config_default_backend_is_honoured_when_the_call_omits_it() {
        let (_home, _env, t) = backend_tool("vps", "peer-0002");
        std::fs::write(
            _home.path().join("config.toml"),
            "[cluster]\ndelegate_backend = \"vps\"\n",
        )
        .unwrap();
        let out = t.call(json!({"goal": "do the work"})).await;
        assert!(
            out.starts_with("error:") && out.contains("a2a_agents"),
            "the default backend resolves the same way an explicit one does: {out}"
        );
    }
}
