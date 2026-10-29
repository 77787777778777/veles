//! Desktop API server — REST + SSE over axum, the surface veles-desktop
//! (Tcl/Tk) speaks.
//!
//! Design (deliberately NOT the gateway bus):
//!   - Desktop is a local loopback-only frontend, not a chat platform.
//!     Bus adapters key sessions by (platform, chat_id) with no history
//!     replay and outbound-only delivery — wrong shape for a window that
//!     opens a session, shows its transcript, streams deltas, and
//!     cancels mid-turn. Desktop sessions are direct `Agent`s keyed by
//!     session id, with real history load/save.
//!   - Streaming is SSE (one HTTP response per turn) instead of a
//!     persistent websocket: tclcurl consumes an incremental body with
//!     -writeproc trivially, reconnect is stateless, and curl can drive
//!     it for tests. hermes' tui_gateway uses request/response-over-WS
//!     with busy→queue locking; SSE with per-session mutexes gets the
//!     same semantics with less machinery.
//!
//! Endpoints (all loopback, JSON in/out, SSE for chat):
//!   GET  /api/health                  liveness + uptime (UNAUTHENTICATED)
//!   GET  /api/sessions                session list (id/title/created/count)
//!   POST /api/sessions/new            create a session → {session_id}
//!   DELETE /api/sessions/{id}         delete a session
//!   GET  /api/sessions/{id}/history   [{role, content, display_reasoning}]
//!   POST /api/chat                    {session_id, text} → SSE stream:
//!        event: route|reasoning|delta|tool|done|error
//!
//! Security: binds 127.0.0.1 by default (single-user local tool;
//! the flake wraps the desktop launcher to the same host) and
//! authenticates every route but `/api/health` with a bearer token
//! (`authn::AuthConfig`, resolved ONCE at serve time so the router and
//! the bind cannot disagree). With no token configured the gateway
//! MINTS one into `<config_dir>/.env` at 0600 and logs the path —
//! loopback is not a trust boundary, it is an address: every local
//! process, and in a container everything that can reach the published
//! port, could otherwise drive config writes, live interpreters, ACL
//! edits and sandbox binds. `VELES_DESKTOP_OPEN=1` declines the token
//! deliberately and says so in the log.

use crate::panes;
use anyhow::Result;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use veles_core::agent::Agent;
use veles_core::config::VelesConfig;
use veles_core::hooks::HookBus;
use veles_core::lockutil::{LockOk, RwLockOk};
use veles_core::providers::{CancelToken, StreamEvent};
use veles_core::store::Store;
use veles_core::tools;

pub use veles_core::config::desktop_port as default_port;

pub fn desktop_enabled(cfg: &VelesConfig) -> bool {
    cfg.platforms
        .get("desktop")
        .and_then(|d| d.get("enabled"))
        .and_then(|e| e.as_bool())
        .unwrap_or(true) // on by default: loopback bind, bearer token
}

/// Shared server state: config, store, hook bus, and one live Agent per
/// open session (created lazily, mirroring the gateway's agent_for).
///
/// Session agents use a TOKIO mutex: the chat task holds the guard
/// across `run_turn().await` inside a `tokio::spawn`, which requires a
/// Send future (std MutexGuard is not Send). The gateway's std mutex is
/// fine because its turn runs inside `block_on`, never spawned.
/// The reverse channel into the desktop's OWN Tcl interpreter.
///
/// The gateway's interpreter is reachable at `/api/tcl/eval`. The
/// desktop window's is not reachable at all: veles-desktop is an HTTP
/// *client* with no listener, so nothing here could reach the process
/// that owns the widgets. That is the gap that stopped an agent from
/// helping with the Dev tab's live scripts — it could edit the files
/// and not see the result.
///
/// So the desktop subscribes (`GET /api/desktop/eval/events`), work is
/// pushed to it, and it posts the answer back. The same shape
/// `/api/lsp/events` and `/api/irc/events` already use, with a
/// correlation id and a parked oneshot per request.
///
/// ARMING is the safety property. Evaluating in the app interpreter can
/// redefine any proc in the running window, so the desktop only accepts
/// it while the user has switched "live patching" on in the Dev pane.
/// The desktop enforces that itself; `armed` here is a mirror of what it
/// last told us, so a request can be refused with a useful message
/// instead of timing out.
struct DesktopEvalBus {
    requests: tokio::sync::broadcast::Sender<Value>,
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>,
    armed: std::sync::atomic::AtomicBool,
}

impl DesktopEvalBus {
    fn new() -> Self {
        let (requests, _) = tokio::sync::broadcast::channel(64);
        Self {
            requests,
            pending: Mutex::new(HashMap::new()),
            armed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn connected(&self) -> bool {
        self.requests.receiver_count() > 0
    }
}

pub struct DesktopServer {
    /// The loaded config, behind a lock so a write to config.toml can be
    /// picked up without restarting.
    ///
    /// This was a bare `Arc<VelesConfig>` captured at boot, and
    /// `agent_for` clones it into every agent it constructs — so an edit
    /// to config.toml did not reach even a session opened a second
    /// later. Now `POST /api/config/reload` swaps it, and each NEW agent
    /// takes the current one. Agents already cached keep the snapshot
    /// they were built with; the reload response says how many, because
    /// a settings pane that implies otherwise is worse than one that
    /// admits the limit.
    cfg: std::sync::RwLock<Arc<VelesConfig>>,

    pub(crate) store: Arc<Mutex<Store>>,
    /// Read-only connections to the store's file, for the listings that
    /// must not take `store`'s lock (see `ReadPool`).
    readers: veles_core::store::ReadPool,
    hooks: Arc<HookBus>,
    bus: Option<crate::gateway::GatewayBus>,
    scheduler: Option<Arc<veles_core::cron::Scheduler>>,
    /// `Arc<Agent>`, not `Arc<Mutex<Agent>>`: `run_turn` takes `&self`.
    /// The lock a turn used to hold for its whole life is now
    /// `turn_locks` below, which serializes turns without also blocking
    /// every command, steer or status read issued during one.
    agents: Mutex<HashMap<String, Arc<Agent>>>,
    /// Per-session turn serialization — the `turn_lock` shape the
    /// gateway already uses. Two `POST /api/chat` on one session must
    /// not interleave their history writes; that used to fall out of
    /// the agent mutex by accident, so removing it had to make it
    /// explicit.
    turn_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// How many windows are watching each session right now (its event
    /// stream is open). A session a window shows is one this process
    /// holds open for session mail, cached agent or not.
    watchers: Mutex<HashMap<String, usize>>,
    started: Instant,
    /// See `DesktopEvalBus`: the channel into the desktop window's own
    /// Tcl interpreter.
    desktop_eval: DesktopEvalBus,
    /// One short-lived, server-owned signed installation preview. The
    /// opaque id is the only handle a desktop client receives.
    pub(crate) signed_download: crate::desktop_install::SignedDownloads,
}

impl DesktopServer {
    /// May the desktop wake this session? It owns every session it
    /// holds, MINUS chat-backed ones — those belong to the gateway (same
    /// process, through `gateway_owns_session`), and a wake for one must
    /// deliver INTO the chat, not a desktop paint.
    ///
    /// "Holds" is `holds_for_mail`'s definition, deliberately: a cached
    /// agent OR a window showing it. The cached-agent map alone is the
    /// wrong question — opening a transcript runs no turn, so it builds
    /// no agent, and `reload_config` empties the map wholesale. A wake
    /// that asked only the map therefore had NO owner for every freshly
    /// opened session: the gateway answers false (not chat-backed), the
    /// desktop answered false (no agent), `run_wake_loop` dropped the
    /// event, and a monitor or background shell only surfaced once the
    /// human typed and pushed its parked event through a real turn's
    /// drain. Two definitions of "held" in one file was the defect; this
    /// is now the one the mail path already trusted.
    pub fn owns_session(&self, session_id: &str) -> bool {
        if crate::gateway::gateway_owns_session(session_id) {
            return false;
        }
        self.agents.lock_ok().contains_key(session_id)
            || self
                .watchers
                .lock_ok()
                .get(session_id)
                .is_some_and(|n| *n > 0)
    }

    /// The desktop's wake half: admit under the per-session turn lock,
    /// run `run_background_wake_turn`, deliver the reply AS A
    /// TRANSCRIPT MESSAGE — the session bus paints it to windows that
    /// have the session open, and history carries it for any that do
    /// not (the desktop has no "last chat" to push a wire message to;
    /// its SSE stream is push enough).
    pub async fn desktop_wake_one(
        &self,
        event: &veles_core::session_wake::WakeEvent,
    ) -> anyhow::Result<()> {
        // The process-wide claim: whichever owner claims first runs
        // the turn; the gateway may race us for a session neither has
        // yet (never happens for held sessions, but cheap truth).
        let mut _claim = match veles_core::delegate::try_claim_wake(&event.session_id) {
            Some(c) => c,
            None => return Ok(()),
        };
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
        let lock = self.turn_lock(&event.session_id);
        let lock_for_spawn = lock.clone();
        // RunRegistration clears the store before the outer surface
        // releases this lock. Wait through that handoff only for the
        // explicit post-turn signal; a busy completion still stands down.
        let _held = if event.kind == "background-wake-pending" {
            lock.lock().await
        } else {
            match lock.try_lock() {
                Ok(g) => g,
                Err(_) => return Ok(()),
            }
        };
        // A user turn may have won the lock while this wake waited.
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
        let agent = self.agent_for(&event.session_id)?;
        if !agent.cfg.agent.injections.background {
            return Ok(());
        }
        let mut history = {
            agent
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .load_history(&event.session_id)
                .unwrap_or_default()
        };
        let res = agent
            .run_background_wake_turn(&mut history, None, |_| {})
            .await;
        _claim.retry_pending_on_drop();
        let _ = res; // the reply is already persisted + bus-published by run_background_wake_turn

        // The wake turn's boundary drains the shared queue, exactly
        // like `chat()`'s drain loop — a parked message runs next, and
        // the wake is the only boundary in sight (no new POST is
        // coming). Mirrors chat(): release OUR completed row, take
        // next, repeat. The spawned drain takes the turn lock itself:
        // dropping OUR guard above means a new POST can claim the
        // session between the wake and the drain — which is intended,
        // the same dance `chat()` does with its own per-POST locks.
        // No asker juggling: the desktop's asker is a session attach
        // (see `agent_for`) — a queued turn asks exactly like a POSTed
        // one, whichever surface is watching.
        let a = agent.clone();
        let sid = event.session_id.clone();
        // The window's language (X-Veles-Locale) does not ride into a
        // spawned task on its own; carry it so the turn's engine text
        // follows the desktop (the session's own language still wins).
        let ui_locale = veles_core::i18n::scoped();
        veles_core::rt::spawn(async move {
            let _turn = lock_for_spawn.lock().await;
            let mut body: Option<(String, i64)> = {
                a.store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .queue_take_next(&sid)
                    .map_err(|e| tracing::warn!("queue: taking {}'s next row failed: {e:#}", sid))
                    .ok()
                    .flatten()
                    .map(|item| (item.body, item.seq))
            };
            while let Some((text, seq)) = body {
                let mut history = a
                    .store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .load_history(&sid)
                    .unwrap_or_default();
                let cancel = veles_core::providers::CancelToken::new();
                let _ = veles_core::i18n::scope(
                    ui_locale.clone(),
                    a.run_turn(&mut history, &text, Some(cancel), |_| {}),
                )
                .await;
                let _ = a
                    .store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .queue_release(&sid, seq);
                body = a
                    .store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .queue_take_next(&sid)
                    .map_err(|e| tracing::warn!("queue: taking {}'s next row failed: {e:#}", sid))
                    .ok()
                    .flatten()
                    .map(|item| (item.body, item.seq));
            }
        });
        Ok(())
    }
    pub fn new(cfg: Arc<VelesConfig>, store: Arc<Mutex<Store>>, hooks: Arc<HookBus>) -> Self {
        // MCP tools (gateway parity): connect on the shared MCP host and
        // register into the deferred registry — every desktop agent then
        // sees them via tool_search/tool_call.
        veles_core::mcp::boot_mcp_deferred(&cfg);
        let readers = veles_core::store::ReadPool::new(&store.lock_ok());
        Self {
            cfg: std::sync::RwLock::new(cfg),
            store,
            readers,
            hooks,
            bus: None,
            scheduler: None,
            agents: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            turn_locks: Mutex::new(HashMap::new()),
            started: Instant::now(),
            desktop_eval: DesktopEvalBus::new(),
            signed_download: crate::desktop_install::SignedDownloads::default(),
        }
    }

    /// The config as currently loaded. Every reader goes through this so
    /// the swap in `reload_config` has exactly one place to happen.
    fn cfg(&self) -> Arc<VelesConfig> {
        self.cfg.read_ok().clone()
    }

    /// Run `f` on a pooled read-only connection (`ReadPool`), so a read
    /// neither waits behind nor holds up the shared store lock. A commit
    /// that waited seconds on a busy disk with that lock held stalled
    /// every desktop read behind it, the event stream's 1.5 s tick in
    /// every open window included, each blocking a tokio worker. Reads
    /// only: a write here fails ("attempt to write a readonly database").
    fn read<T>(&self, who: &'static str, f: impl FnOnce(&Store) -> T) -> T {
        self.readers.read(&self.store, who, f)
    }

    /// Re-read config.toml into the live handle, then EVICT every
    /// cached agent (R4.5). Eviction is safe now because a session's
    /// overrides live in the store, not in the agent — the next
    /// `agent_for` rebuilds on the fresh config and re-seeds the
    /// override row. A turn in flight keeps its `Arc`, which is right:
    /// a turn belongs to the config it started with. Returns the number
    /// of agents that will keep their old snapshot — running turns,
    /// always transient.
    pub(crate) fn reload_config(&self) -> Result<usize> {
        let fresh = Arc::new(veles_core::config::load()?);
        // `tui.locale` is the process DEFAULT language. When the file's
        // value changes (the desktop's "Make default", `/locale primary`,
        // a hand edit), the default follows it now rather than at the next
        // restart. Only on a CHANGE of the stored value: a reload for any
        // other edit must not undo a `/locale <id>` switch on a
        // single-user surface.
        let before = self.cfg.read_ok().tui.locale.clone();
        if fresh.tui.locale != before {
            let now = veles_core::i18n::init(fresh.tui.locale.as_deref());
            tracing::info!("config: tui.locale changed — default language is now {now}");
        }
        *self.cfg.write_ok() = fresh;
        // Evicting the agents cache also drops each agent's attached
        // asker (Weak dies with the Arc). A MID-TURN agent keeps its
        // Arc (running turns belong to the config they started with),
        // so its steer slot and cancel registration stay live — a
        // config edit must not eat a running turn's steering, which
        // the old `steers.clear()` here did silently (PENDING
        // 2026-09-14: a steer queued before the clear died with zero
        // signal). Docket steers survive by design; the next turn
        // drains them.
        self.agents.lock_ok().clear();
        Ok(0)
    }

    /// Watch config.toml and reload on a hand edit — the same shape
    /// tclrt.rs uses for the plugins directory (same `notify` crate,
    /// same access-event filter). A config the server only re-reads
    /// when asked is a config the settings UI and a $EDITOR fight over.
    fn spawn_config_watcher(self: &Arc<Self>) {
        use notify::{EventKind, RecursiveMode, Watcher};
        let path = veles_core::config::config_dir().join("config.toml");
        let srv = self.clone();
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let Ok(mut watcher) = notify::recommended_watcher(tx) else {
                return;
            };
            if watcher.watch(&path, RecursiveMode::NonRecursive).is_err() {
                return;
            }
            for res in rx {
                let Ok(ev) = res else { continue };
                if matches!(ev.kind, EventKind::Access(_)) {
                    continue;
                }
                if let Some(p) = ev.paths.first() {
                    if p != &path {
                        continue;
                    }
                }
                // Debounce: editors write in several events; one beat
                // of quiet is enough.
                std::thread::sleep(std::time::Duration::from_millis(300));
                let _ = srv.reload_config();
                tracing::info!("config.toml changed — reloaded");
            }
        });
    }

    /// Attach the gateway bus (platform status for /api/status).
    pub fn with_bus(mut self, bus: crate::gateway::GatewayBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Attach the cron scheduler (manual job runs from the desktop).
    pub fn with_scheduler(mut self, sched: Arc<veles_core::cron::Scheduler>) -> Self {
        self.scheduler = Some(sched);
        self
    }

    fn bus_platforms(&self) -> Vec<String> {
        self.bus
            .as_ref()
            .map(|b| b.outbound_platforms())
            .unwrap_or_default()
    }

    /// Lazily build (or fetch) the agent for a session id. Errors when
    /// the session doesn't exist in the store — desktop never invents
    /// sessions implicitly.
    /// Per-session turn lock — arrival order within one session,
    /// concurrency across sessions. See `turn_locks`.
    fn turn_lock(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.turn_locks
            .lock_ok()
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// A window is watching `session_id` until the guard drops (its event
    /// stream closes).
    fn watch(self: &Arc<Self>, session_id: &str) -> WatchGuard {
        *self
            .watchers
            .lock_ok()
            .entry(session_id.to_string())
            .or_insert(0) += 1;
        WatchGuard {
            srv: self.clone(),
            sid: session_id.to_string(),
        }
    }

    /// Does this desktop hold the session for session mail: a cached
    /// agent, or a window showing it — and not one of the gateway's.
    /// One definition, shared with the wake owner: `owns_session` above.
    fn holds_for_mail(&self, session_id: &str) -> bool {
        self.owns_session(session_id)
    }

    fn agent_for(&self, session_id: &str) -> Result<Arc<Agent>> {
        let mut agents = self.agents.lock_ok();
        if let Some(a) = agents.get(session_id) {
            return Ok(a.clone());
        }
        {
            let store = self.store.lock_ok();
            if !store.session_exists(session_id)? {
                anyhow::bail!("session not found: {session_id}");
            }
        }
        // ONE-TIME MIGRATION (R4.5): the desktop's old
        // `override:<sid>` keys in desktop_state.json move into the
        // store row the first time this session is opened here, so an
        // upgrade does not silently forget a user's model choice. The
        // key is left in place (harmless, and this must be idempotent).
        // Application itself happens in `Agent::new` now — every
        // surface, not just this server's cache.
        {
            let key = format!("override:{session_id}");
            let have_row = self
                .store
                .lock_ok()
                .override_get(session_id)
                .ok()
                .flatten()
                .is_some_and(|o| {
                    o.provider.is_some() || o.model.is_some() || o.reasoning.is_some()
                });
            if !have_row {
                if let Some(ov) = load_desktop_state().get(&key).cloned() {
                    let get = |k: &str| {
                        ov.get(k)
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    };
                    let (p, m, r) = (get("provider"), get("model"), get("reasoning"));
                    if p.is_some() || m.is_some() || r.is_some() {
                        let _ = self.store.lock_ok().override_replace(
                            session_id,
                            p.as_deref(),
                            m.as_deref(),
                            r.as_deref(),
                        );
                    }
                }
            }
        }

        // Desktop sessions run the full interactive registry (no cronjob
        // recursion guard needed — the desktop IS the operator surface).
        let registry = Arc::new(tools::builtin_registry(
            self.store.clone(),
            Some(format!("desktop:{session_id}")),
            session_id,
        ));
        let agent = Agent::new(
            self.cfg(),
            self.hooks.clone(),
            registry,
            self.store.clone(),
            session_id.to_string(),
        )?
        .with_surface("desktop");

        // The desktop's asker is a session ATTACH, not a per-turn slot:
        // it parks asks in the registry (answerable over REST by any
        // surface, relayed to every other attached surface) and lives
        // until the server drops the agent. The per-turn
        // set-at-head/clear-at-tail cycle is what let one turn's cleanup
        // erase the gateway's standing ChatAsker for the same session id
        // (PENDING 2026-09-14).
        let agent = Arc::new(agent);
        let _asker_id = agent.set_asker(Arc::new(DesktopAsker {
            session_id: session_id.to_string(),
        }));
        agents.insert(session_id.to_string(), agent.clone());
        Ok(agent)
    }
}

/// Asks the desktop client: the question rides the session bus, the
/// answer comes back over REST.
///
/// The asker is keyed by SESSION, not by one request's channel — there is
/// no request channel any more, since `POST /api/chat` is fire-and-forget.
/// That is the whole fix for "closing the window auto-denies the next
/// approval": an approval is a property of the turn, which outlives any
/// single view of it, so it is published to whoever is watching and PARKED
/// in the `veles_core::ask` registry. A reattaching client picks it up from
/// `GET /api/asks`. No viewers is the normal state of a background turn,
/// not a denial.
pub(crate) struct DesktopAsker {
    pub(crate) session_id: String,
}

#[async_trait::async_trait]
impl veles_core::ask::Asker for DesktopAsker {
    async fn ask(&self, req: veles_core::ask::AskRequest) -> veles_core::ask::AskAnswer {
        // Register FIRST: from this moment the ask is answerable over REST
        // and visible to `GET /api/asks`, so it survives every viewer
        // leaving. On a RELAYED ask (another surface already registered
        // this id — the chat leg did), `register` returns the same ask's
        // new leg and the bindings stay the original's.
        let Some(leg) = veles_core::ask::register(&req, None, None) else {
            // The ask already ended elsewhere between relay and render —
            // nothing to show, nothing to park.
            return veles_core::ask::AskAnswer::timeout();
        };
        // Publish on the session bus — the same one `run_turn` feeds, so
        // an approval reaches a second window, a reattach, and a surface
        // that did not start the turn. One conversion (`publish_stream`),
        // one vocabulary.
        veles_core::session_tap::publish_stream(
            &self.session_id,
            &StreamEvent::Ask(Box::new(req.clone())),
        );
        leg.wait().await
    }
    /// A window on the operator's machine, and its secret field posts
    /// to a loopback-only route.
    fn private_input(&self) -> bool {
        true
    }
}

/// The desktop as a turn-owning surface, for cron session wakeups:
/// the sessions it holds an agent for, minus the gateway's (the same
/// split `owns_session` already draws for the wake owner). Registered
/// next to the wake loop, where the `Arc<DesktopServer>` exists.
pub struct DesktopSessionRunner {
    srv: Arc<DesktopServer>,
}

impl veles_core::cron::SessionRunner for DesktopSessionRunner {
    fn owns_session(&self, session_id: &str) -> bool {
        self.srv.owns_session(session_id)
    }

    fn try_lock_session(&self, session_id: &str) -> Option<veles_core::cron::SessionLockGuard> {
        let lock = self.srv.turn_lock(session_id);
        // `try_lock_owned`: the guard travels back to the scheduler,
        // which holds it across an `.await` — a borrowed guard cannot
        // outlive this call.
        match lock.try_lock_owned() {
            Ok(g) => Some(veles_core::cron::SessionLockGuard::new(g)),
            Err(_) => None, // a turn is running; its drain covers the queue
        }
    }

    fn kick_queue(&self, session_id: &str) {
        // A cron turn just ended under our turn lock: a message that
        // parked during it has no boundary left to wait for (the
        // boundary was the cron turn's, and its drainer is not ours).
        // Spawn `desktop_wake_one`'s post-turn drain loop exactly —
        // take the lock, run rows in seq order, release each after its
        // turn. Spawned because the kick is called from the
        // scheduler's sync context and the lock may be re-acquired by
        // a racing POST (intended: the same dance the wake path does).
        let srv = self.srv.clone();
        let sid = session_id.to_string();
        // The window's language (X-Veles-Locale) does not ride into a
        // spawned task on its own; carry it so the turn's engine text
        // follows the desktop (the session's own language still wins).
        let ui_locale = veles_core::i18n::scoped();
        veles_core::rt::spawn(async move {
            let lock = srv.turn_lock(&sid);
            let _turn = lock.lock().await;
            let agent = match srv.agent_for(&sid) {
                Ok(a) => a,
                Err(_) => return, // session vanished since the kick
            };
            let mut body: Option<(String, i64)> = srv
                .store
                .lock_ok()
                .queue_take_next(&sid)
                .map_err(|e| tracing::warn!("queue: taking {sid}'s next row failed: {e:#}"))
                .ok()
                .flatten()
                .map(|item| (item.body, item.seq));
            while let Some((text, seq)) = body {
                let mut history = srv.store.lock_ok().load_history(&sid).unwrap_or_default();
                let cancel = veles_core::providers::CancelToken::new();
                let _ = veles_core::i18n::scope(
                    ui_locale.clone(),
                    agent.run_turn(&mut history, &text, Some(cancel), |_| {}),
                )
                .await;
                let _ = srv.store.lock_ok().queue_release(&sid, seq);
                body = srv
                    .store
                    .lock_ok()
                    .queue_take_next(&sid)
                    .map_err(|e| tracing::warn!("queue: taking {}'s next row failed: {e:#}", sid))
                    .ok()
                    .flatten()
                    .map(|item| (item.body, item.seq));
            }
        });
    }
}

/// One window watching one session; drops with its event stream.
struct WatchGuard {
    srv: Arc<DesktopServer>,
    sid: String,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        let mut w = self.srv.watchers.lock_ok();
        if let Some(n) = w.get_mut(&self.sid) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                w.remove(&self.sid);
            }
        }
    }
}

/// The desktop's sessions as ones this process holds open (session mail,
/// `veles_core::presence`): the agents it has cached and the sessions a
/// window shows, minus the gateway's. A nudge is `kick_queue` — the drain
/// loop builds the agent if need be and takes the turn lock, so a session
/// mid-turn runs the mail right after.
impl veles_core::presence::PresenceSource for DesktopSessionRunner {
    fn open_sessions(&self) -> Vec<veles_core::presence::OpenSession> {
        let mut ids: Vec<String> = self.srv.agents.lock_ok().keys().cloned().collect();
        for sid in self.srv.watchers.lock_ok().keys() {
            if !ids.contains(sid) {
                ids.push(sid.clone());
            }
        }
        ids.into_iter()
            .filter(|sid| !crate::gateway::gateway_owns_session(sid))
            .map(|sid| veles_core::presence::OpenSession {
                session_id: sid,
                surface: "desktop".into(),
            })
            .collect()
    }

    fn nudge(&self, session_id: &str) -> bool {
        if !self.srv.holds_for_mail(session_id) {
            return false;
        }
        veles_core::cron::SessionRunner::kick_queue(self, session_id);
        true
    }
}

/// The desktop server as a wake owner (`session_wake::WakeOwner`):
/// sessions the desktop holds and the gateway does not claim.
struct DesktopWakeOwner {
    srv: Arc<DesktopServer>,
}

impl veles_core::session_wake::WakeOwner for DesktopWakeOwner {
    fn own_sid(&self, session_id: &str) -> bool {
        self.srv.owns_session(session_id)
    }

    fn admit_and_run(
        &self,
        event: veles_core::session_wake::WakeEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let srv = self.srv.clone();
        Box::pin(async move {
            if let Err(e) = srv.desktop_wake_one(&event).await {
                tracing::warn!(
                    "desktop background-wake failed for {}: {e:#}",
                    event.session_id
                );
            }
        })
    }
}

#[derive(Deserialize)]
struct DecideRequest {
    /// A choice key ("once"/"deny"/…) or, for clarify, free text.
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    text: Option<String>,
    /// Multi-select / multi-question answers (2026-09-26 parity):
    /// choice keys for one question, or per-question objects
    /// {"q1":{"selected":["c1"],"text":"…"}, …}.
    #[serde(default)]
    answers: Option<serde_json::Value>,
}

/// Answer an open question. Returns ok=false when it already expired.
async fn decide_ask(Path(id): Path<String>, Json(req): Json<DecideRequest>) -> Json<Value> {
    let answer = if let Some(ref v) = req.answers {
        match veles_core::ask::answers_value(v) {
            Some(a) => a,
            None => return Json(json!({ "ok": false, "error": "bad answers payload" })),
        }
    } else {
        match (req.choice, req.text) {
            // S10: choice + text together — the desktop amend path answers
            // "allow once" WITH free text; an "amend: {...}" text is the
            // approval-time input rewrite.
            (Some(c), Some(t)) if !c.trim().is_empty() && !t.trim().is_empty() => {
                veles_core::ask::AskAnswer {
                    selected: vec![c.trim().to_string()],
                    free_text: Some(t.trim().to_string()),
                    timed_out: false,
                    ..Default::default()
                }
            }
            (Some(c), _) if !c.trim().is_empty() => veles_core::ask::AskAnswer::choice(c.trim()),
            (_, Some(t)) => veles_core::ask::AskAnswer::text(t.trim()),
            _ => veles_core::ask::AskAnswer::choice(veles_core::ask::CHOICE_DENY),
        }
    };
    let ok = veles_core::ask::resolve(&id, answer);
    Json(json!({ "ok": ok }))
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// Engine logs (the same ring the TUI's F2 pane reads). The desktop has
/// no terminal to watch, so this is the only way to see what the gateway
/// is doing without tailing a file.
async fn logs(Query(q): Query<LogQuery>) -> Json<Value> {
    let lines = veles_core::logbuf::snapshot(q.limit.unwrap_or(200).min(veles_core::logbuf::CAP));
    Json(json!({ "logs": lines, "path": veles_core::logbuf::log_path() }))
}

/// Themes, resolved by the Rust side (built-ins + ~/.config/veles/themes/*.toml).
///
/// The Tk app used to carry its own parallel palettes in Tcl, kept in
/// sync with these by hand — same preset names, different key names,
/// two files per theme. tcllib has no TOML parser, so instead of
/// teaching Tcl to read TOML the gateway serves the resolved palettes
/// as JSON and the desktop consumes them with the `json` package it
/// already uses.
async fn themes() -> Json<Value> {
    Json(json!({ "themes": veles_core::theme::all() }))
}

/// Open questions — lets a reloaded client redraw what it missed.
async fn list_asks() -> Json<Value> {
    Json(json!({ "asks": veles_core::ask::open_all() }))
}

#[derive(Deserialize)]
struct CheckpointQuery {
    #[serde(default)]
    session: Option<String>,
}

/// Undo points (M13), newest first.
async fn list_checkpoints(
    State(srv): State<Arc<DesktopServer>>,
    Query(q): Query<CheckpointQuery>,
) -> Json<Value> {
    let items = srv
        .read("desktop::checkpoints", |s| {
            s.checkpoint_list(q.session.as_deref(), 100)
        })
        .unwrap_or_default();
    let rows: Vec<Value> = items
        .iter()
        .map(|c| {
            json!({
                "id": c.id, "session_id": c.session_id, "path": c.path,
                "bytes": c.bytes, "tool": c.tool, "created_at": c.created_at,
                "existed": c.blob.is_some(),
            })
        })
        .collect();
    Json(json!({ "checkpoints": rows }))
}

/// Put a file back. Answers with what happened, including failures —
/// the client shows it verbatim.
async fn restore_checkpoint(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<i64>,
) -> Json<Value> {
    match veles_core::checkpoints::restore(&srv.store, id) {
        Ok(msg) => Json(json!({ "ok": true, "message": msg })),
        Err(e) => Json(json!({ "ok": false, "message": format!("{e:#}") })),
    }
}

/// The session's task list (M12) — the desktop renders it as a panel.
///
/// Carries plan_mode and the last presented plan in the SAME response:
/// the desktop's Tasks pane shows all three together, and three
/// endpoints for one panel is three requests per refresh when the pane
/// polls every few seconds.
async fn session_todos(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let (items, plan_mode, plan) = srv.read("desktop::todos", |s| {
        (
            s.todos_list(&id).unwrap_or_default(),
            veles_core::plan::mode_for(s, &id) == veles_core::plan::Mode::Plan,
            veles_core::plan::presented_plan(s, &id),
        )
    });
    Json(json!({
        "todos": items,
        "plan_mode": plan_mode,
        "plan": plan,
    }))
}

#[derive(Deserialize)]
struct ChatRequest {
    session_id: String,
    /// An active target-node join id, supplied by the desktop.
    #[serde(default)]
    join_id: Option<String>,
    text: String,
    /// Files the user attached — paths or URLs. Folded into the message
    /// body as `MEDIA:` markers, the same convention a reply uses, so
    /// they persist with the message and render in the transcript
    /// without a second mechanism.
    #[serde(default)]
    attachments: Vec<String>,
}

async fn health(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "uptime_s": srv.started.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
        "profile": veles_core::profiles::active_profile(),
    }))
}

/// `GET /api/sessions` — one page of sessions, newest activity first.
///
/// `limit` (default 100, at most 1000), `offset`, `q` (title, id or first
/// user line; with `fts=1` the message text), `titled=titled|untitled`,
/// and `include=<id>,<id>` for sessions wanted whatever page they fall
/// on (the sidebar's pins). The reply carries `total`, the number of
/// sessions matching, so a list can say how much it is not showing —
/// the old fixed 100 newest let a batch run of 1,770 sessions push
/// everything else out of reach (2026-10-04).
async fn list_sessions(
    State(srv): State<Arc<DesktopServer>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let num = |k: &str, d: u32| q.get(k).and_then(|v| v.parse::<u32>().ok()).unwrap_or(d);
    let query = veles_core::store::SessionQuery {
        limit: num("limit", 100).clamp(1, 1000),
        offset: num("offset", 0),
        q: q.get("q").cloned().filter(|v| !v.trim().is_empty()),
        fts: q.get("fts").is_some_and(|v| v == "1" || v == "true"),
        titled: match q.get("titled").map(String::as_str) {
            Some("titled") => Some(true),
            Some("untitled") => Some(false),
            _ => None,
        },
    };
    let include: Vec<String> = q
        .get("include")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // `remote=1` (the desktop since 2026-10-06): also the sessions other
    // nodes own, read from their replicas here, read-only. The local page
    // is then fetched from 0 so the MERGED list can be paged. An older
    // client never asks, and its listing is unchanged.
    let remote = q.get("remote").is_some_and(|v| v == "1" || v == "true");
    let local_query = if remote {
        veles_core::store::SessionQuery {
            offset: 0,
            limit: query.offset.saturating_add(query.limit).min(100_000),
            ..query.clone()
        }
    } else {
        query.clone()
    };
    // Off the async workers, on a read-only connection of its own. On
    // 2026-10-13 this listing held the shared store lock for 30–79 s
    // while a build had the disk, and every other desktop request
    // waited behind it.
    let listed = veles_core::rt::spawn_blocking(move || {
        srv.readers
            .read(&srv.store, "desktop::list_sessions", |store| {
                list_sessions_on(store, &query, &local_query, include, remote)
            })
    })
    .await;
    match listed {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("listing sessions: {e}") })),
        ),
    }
}

/// `list_sessions`'s reads, on whichever connection `ReadPool` handed it.
fn list_sessions_on(
    store: &Store,
    query: &veles_core::store::SessionQuery,
    local_query: &veles_core::store::SessionQuery,
    include: Vec<String>,
    remote: bool,
) -> anyhow::Result<Value> {
    let (mut rows, mut total) = store.session_page(local_query)?;
    let missing: Vec<String> = include
        .into_iter()
        .filter(|id| !rows.iter().any(|r| &r.id == id))
        .collect();
    let pinned_extra = store.session_rows(&missing).unwrap_or_default();
    // Cross-process liveness (R4.5): which sessions have a turn in
    // flight, on which surface, in which process — read from the
    // session_runs registry, not from this server's own bus (which
    // only ever knew about ITS turns — the original "TUI session not
    // shown running in the desktop" report). Read without reaping: a
    // GET must not be a write.
    let runs: std::collections::HashMap<String, veles_core::store::SessionRun> = store
        .runs_current(30)
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.session_id.clone(), r))
        .collect();
    // Replica rows come from their own files, and each is checked
    // against this store: a session this node has itself is local.
    let mut owner: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    if remote {
        let found = veles_core::storesync::replica_session_rows(query, |_| false);
        let mut names: std::collections::HashMap<String, String> = Default::default();
        let mut kept = Vec::new();
        for r in found {
            if store.session_exists(&r.row.id).unwrap_or(false) {
                continue;
            }
            let name = names
                .entry(r.node.clone())
                .or_insert_with(|| {
                    store
                        .node_get(&r.node)
                        .ok()
                        .flatten()
                        .map(|n| n.name)
                        .unwrap_or_else(|| r.node.chars().take(8).collect())
                })
                .clone();
            owner.insert(r.row.id.clone(), (r.node.clone(), name));
            kept.push(r.row);
        }
        total += kept.len() as u64;
        rows.extend(kept);
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        rows = rows
            .into_iter()
            .skip(query.offset as usize)
            .take(query.limit as usize)
            .collect();
    }
    rows.extend(pinned_extra);
    let sessions: Vec<Value> = rows
        .into_iter()
        .map(|m| {
            // The fork link: a subagent child names its parent here, so
            // the sidebar can mark children and offer "go to parent"
            // (USER taxonomy, 2026-09-17).
            let mut v = json!({
                "id": m.id,
                "title": m.title,
                "preview": m.preview,
                "parent": m.parent,
                "created_at": m.created_at,
                "updated_at": m.updated_at,
                "message_count": m.message_count,
                "running": false,
            });
            if let Some(r) = runs.get(&m.id) {
                v["running"] = json!(true);
                v["surface"] = json!(r.surface);
                v["pid"] = json!(r.pid);
                v["host"] = json!(r.host);
            }
            if let Some((node, name)) = owner.get(&m.id) {
                v["remote"] = json!(true);
                v["node"] = json!(node);
                v["node_name"] = json!(name);
            }
            v
        })
        .collect();
    Ok(json!({ "sessions": sessions, "total": total, "offset": query.offset }))
}

/// `POST /api/sessions/delete {"ids": [...]}` — delete many sessions in
/// one transaction (see `Store::delete_sessions`).
async fn delete_sessions(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let ids: Vec<String> = body["ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "ids: a non-empty array of session ids" })),
        );
    }
    for id in &ids {
        if let Some(a) = srv.agents.lock_ok().remove(id) {
            veles_core::ask::clear_session_asker(id);
            a.forget_last_prompt();
        }
        veles_core::ask::discard_docked_steers(id);
    }
    match srv.store.lock_ok().delete_sessions(&ids) {
        Ok(n) => (StatusCode::OK, Json(json!({ "deleted": n }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// `GET /api/sessions/:id/head?n=` — the opening of a session (its first
/// user and assistant messages, cut short), for a browser's preview.
async fn session_head(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let n = q
        .get("n")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(16)
        .clamp(1, 100);
    // A session another node owns: its head from the replica here.
    let local = srv
        .read("desktop::head", |s| s.session_exists(&id))
        .unwrap_or(true);
    let head = if local {
        srv.read("desktop::head", |s| s.session_head(&id, n))
    } else {
        match veles_core::storesync::replica_holding(&id) {
            Some((_, replica)) => replica.session_head(&id, n),
            None => srv.read("desktop::head", |s| s.session_head(&id, n)),
        }
    };
    match head {
        Ok(rows) => (
            StatusCode::OK,
            Json(json!({
                "messages": rows
                    .into_iter()
                    .map(|(role, content)| json!({ "role": role, "content": content }))
                    .collect::<Vec<_>>(),
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// The transcript of a session ANOTHER node owns, read from its replica
/// here — `None` when this node has the session itself (or no replica
/// does). Read-only: the reply says so and names the owner, and the
/// desktop then opens no event stream and disables the composer; a turn
/// would be refused anyway (`/api/chat` answers "lives on node …").
fn replica_history(srv: &DesktopServer, id: &str) -> Option<(StatusCode, Json<Value>)> {
    if srv
        .read("desktop::history", |s| s.session_exists(id))
        .unwrap_or(true)
    {
        return None;
    }
    let (node, r) = veles_core::storesync::replica_holding(id)?;
    let node_name = srv
        .read("desktop::history", |s| s.node_get(&node))
        .ok()
        .flatten()
        .map(|n| n.name)
        .unwrap_or_else(|| node.chars().take(8).collect());
    let rows = history_rows(&r, &srv.cfg.read_ok(), id, 0, 0);
    Some(match rows {
        Ok(out) => (
            StatusCode::OK,
            Json(json!({
                "messages": out,
                "last_msg_id": r.last_msg_id(id).unwrap_or(0),
                "last_note_id": r.last_note_id(id).unwrap_or(0),
                "read_only": true,
                "node": node,
                "node_name": node_name,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    })
}

/// POST /api/sessions/new — create a session, optionally pre-seeded.
///
/// Body is optional. `{"title": …}` names it, and `{"preamble": …}`
/// records a system message the session opens with. That is what a
/// PRESET session needs — the Dev-tab session has a page of standing
/// context (where the sources live, what `tcl_eval`'s two targets mean,
/// the arm gate) that belongs in the transcript rather than being typed
/// as a fake first user message every time.
async fn new_session(
    State(srv): State<Arc<DesktopServer>>,
    body: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let sid = crate::chat::new_session_id();
    if let Err(e) = srv.store.lock_ok().create_session(&sid) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        );
    }
    if let Some(title) = body["title"].as_str().filter(|t| !t.trim().is_empty()) {
        let _ = srv.store.lock_ok().set_title(&sid, title);
    }
    // Recorded as a `system` message, so it renders as standing context
    // rather than pretending the user asked for it.
    if let Some(preamble) = body["preamble"].as_str().filter(|p| !p.trim().is_empty()) {
        let msg = veles_core::providers::ChatMessage {
            role: "system".into(),
            content: preamble.to_string(),
            reasoning: None,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: None,
            attachments: Vec::new(),
            injection: None,
            msg_id: None,
        };
        if let Err(e) = srv.store.lock_ok().append_message(&sid, &msg) {
            tracing::warn!("session {sid}: preamble not recorded: {e:#}");
        }
    }
    (StatusCode::CREATED, Json(json!({ "session_id": sid })))
}

async fn delete_session(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    // Drop the cached agent (and with it the desktop's attached asker —
    // its Weak in the registry dies with the Arc). A deleted session's
    // docked steers go too; its queue rows go with the store delete.
    if let Some(a) = srv.agents.lock_ok().remove(&id) {
        veles_core::ask::clear_session_asker(&id);
        a.forget_last_prompt();
    }
    veles_core::ask::discard_docked_steers(&id);
    match srv.store.lock_ok().delete_session(&id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "deleted": id }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// The transcript as a client renders it — optionally only the part
/// after `since`.
///
/// ONE renderer for the full load and for a resumed stream's replay.
/// They were separate code paths before (the reload ladder in the
/// history endpoint, the live ladder in the chat SSE handler), and every
/// difference between them was a bug someone reported: tool rounds in a
/// different position, a blank line present live and absent on reload,
/// reasoning that survived one path and not the other. Two renderers of
/// one transcript is the defect; this is the fix.
///
/// `since` is a `msg_id`. 0 means "everything".
///
/// KNOWN EDGE (fixed 2026-09-21): notes used to be replayed by their
/// anchor (`after_msg_id > since`) rather than their own id, so a note
/// anchored at or before the cursor but CREATED after it was never
/// replayed. `since_note` is the client's note-side cursor: a note
/// whose own id is at/below it has been rendered before, and it keeps
/// its anchor position in the walk below regardless.
///
/// `store` is a connection, usually a pooled read-only one
/// (`DesktopServer::read`). Its four reads share one read transaction,
/// so a row written in between cannot be in one list and missing from
/// the others; they took the store lock four times before.
#[allow(clippy::too_many_arguments)]
fn history_rows(
    store: &Store,
    cfg: &veles_core::config::VelesConfig,
    id: &str,
    since: i64,
    since_note: i64,
) -> anyhow::Result<Vec<Value>> {
    let snapshot = if store.conn().is_autocommit() {
        Some(store.conn().unchecked_transaction()?)
    } else {
        None
    };
    let msgs = store.load_history_since(id, since)?;
    // Engine notes are pinned to the message they followed, so they can
    // be rendered back AT THE POINT THEY HAPPENED rather than dumped at
    // the end. Without this a reopened session shows a conversation that
    // looks like you had it — no sign a cron ran it, or that history was
    // compacted underneath you.
    let ids = store.message_ids_since(id, since).unwrap_or_default();
    let notes = store.notes_for(id).unwrap_or_default();
    // When each row happened, for the transcript's timestamps: zipped like
    // `ids` (same filter), sent as RFC 3339 UTC — the client shows it in
    // its own local time.
    let times = store.message_times_since(id, since).unwrap_or_default();
    drop(snapshot);
    let note_json = |n: &veles_core::store::StoredNote| {
        let mut v = json!({
            "role": "note",
            "note_id": n.id,
            "kind": n.kind,
            "summary": n.summary,
            "detail": n.detail,
            "at": sqlite_time_rfc3339(&n.created_at),
        });
        if let Ok(t) = serde_json::from_str::<Value>(&n.toggle) {
            v["toggle"] = t;
        }
        v
    };

    let mut out: Vec<Value> = Vec::new();
    // Notes pinned before any message (after_msg_id 0) come first. The
    // gate is the NOTE cursor: a pre-message note created after the
    // client synced still has to appear on reattach.
    let mut ni = 0usize;
    while ni < notes.len() && notes[ni].after_msg_id <= 0 {
        if notes[ni].id > since_note {
            out.push(note_json(&notes[ni]));
        }
        ni += 1;
    }

    let rendered: Vec<Value> = msgs
        .iter()
        .map(|m| {
            // Prefer display_reasoning (plugin-summarized); fall back to
            // raw reasoning so the block shows even without the plugin.
            let reasoning = m.display_reasoning.clone().or_else(|| m.reasoning.clone());
            if m.role == "tool" {
                // Tool results were silently dropped here — the transcript
                // showed an answer with no trace of the calls that produced
                // it. A `role="tool"` row carries tool_name + the result.
                let name = m
                    .tool_meta
                    .as_ref()
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                // The CALL ID joins this result to the assistant row that
                // made the call (whose `tool_calls` carry the arguments).
                // Without it a reopened session can only render "⚙ name"
                // — the invocation is visible live and gone the moment
                // the session is reloaded. USER: "terminal doesn't show
                // the command ran … not available in historical calls".
                let tcid = m
                    .tool_meta
                    .as_ref()
                    .map(|t| t.tool_call_id.clone())
                    .unwrap_or_default();
                json!({
                    "role": m.role,
                    "tool_name": name,
                    "tool_call_id": tcid,
                    "content": m.content,
                })
            } else {
                let mut v = json!({
                    "role": m.role,
                    "content": m.content,
                    "display_reasoning": reasoning,
                });
                // The injection class, when this row is one (steer /
                // background / compaction / todo / hook / verification)
                // — the transcript renders these as labelled blocks,
                // never as the user's own words.
                if let Some(inj) = &m.injection {
                    v["injection"] = json!(inj);
                    // Skip for steer and compaction: the user's own
                    // words and the structural summary are never muted.
                    if let Some((key, value, help)) = injection_toggle(inj, cfg) {
                        v["toggle"] = json!({
                            "key": key,
                            "value": value,
                            "choices": [],
                            "help": help,
                        });
                    }
                }
                // The model's own tool_calls, exactly as they arrived on
                // the wire — the only persistent copy of the arguments.
                // Emitted ONLY when present so prose-only rows stay small.
                if let Some(tc) = &m.tool_calls {
                    v["tool_calls"] = tc.clone();
                }
                v
            }
        })
        .collect();

    // Walk the messages, dropping in each note once its anchor message
    // has been emitted.
    for (i, mut m) in rendered.into_iter().enumerate() {
        let this_id = ids.get(i).copied().unwrap_or(i64::MAX);
        if let Some(t) = times.get(i) {
            m["at"] = json!(sqlite_time_rfc3339(t));
        }
        if this_id > since {
            // A replayed user row is the optimistic prompt the desktop
            // may already have painted. Give the renderer its persisted
            // identity so turn-started/history and the final reconcile
            // agree that they are ONE row, even after a transcript rebuild.
            if m["role"] == "user" {
                m["msg_id"] = json!(this_id);
            }
            out.push(m);
        }
        while ni < notes.len() && notes[ni].after_msg_id <= this_id {
            // The note-cursor gate: `since_note` says how far the note
            // list itself was rendered, INDEPENDENT of the message
            // anchor. A note can anchor behind the playhead (so the
            // anchor gate must not skip it) yet be freshly created
            // (so the message cursor must not duplicate it): the
            // client's note cursor is the only honest discriminator,
            // so a replayed note carries its id for the client to use.
            if notes[ni].id > since_note {
                out.push(note_json(&notes[ni]));
            }
            ni += 1;
        }
    }
    // Anything pinned past the last message (a note from a turn whose
    // messages were never persisted) still has to be shown.
    while ni < notes.len() {
        if notes[ni].id > since_note {
            out.push(note_json(&notes[ni]));
        }
        ni += 1;
    }
    Ok(out)
}

/// SQLite's `datetime('now')` text ("2026-09-29 14:05:09", UTC, no zone)
/// as RFC 3339 ("2026-09-29T14:05:09Z"). Anything else — already RFC
/// 3339, or empty — passes through unchanged.
fn sqlite_time_rfc3339(t: &str) -> String {
    match chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S") {
        Ok(dt) => dt.and_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        Err(_) => t.to_string(),
    }
}

/// The mute knob an injection row renders next to itself (RM3-S4/A4).
/// Steer and compaction return None: the user's own words and the
/// structural summary of compressed history are never muteable.
fn injection_toggle(
    inj: &str,
    cfg: &veles_core::config::VelesConfig,
) -> Option<(&'static str, String, &'static str)> {
    let (key, on, help) = match inj {
        "background" => (
            "agent.injections.background",
            cfg.agent.injections.background,
            "background completions enter the model's context",
        ),
        "todo" => (
            "agent.injections.todo",
            cfg.agent.injections.todo,
            "the open todo list re-enters context after compaction",
        ),
        "plan" => (
            "agent.injections.plan",
            cfg.agent.injections.plan,
            "the plan this session presented re-enters context after compaction",
        ),
        "hook" => (
            "agent.injections.hook",
            cfg.agent.injections.hook,
            "a compact_start hook's context enters the conversation",
        ),
        "irc_context" => (
            "agent.injections.irc_context",
            cfg.agent.injections.irc_context,
            "IRC channel context entered the conversation",
        ),
        "verification" => (
            "[agent.guards] verification_nudge",
            cfg.agent.guards.verification_nudge,
            "the checker may inject a verification nudge",
        ),
        _ => return None,
    };
    Some((key, on.to_string(), help))
}

async fn history(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    // Off the async workers, on a pooled connection: a whole transcript
    // neither waits for the store lock nor holds it.
    veles_core::rt::spawn_blocking(move || {
        if let Some(reply) = replica_history(&srv, &id) {
            return reply;
        }
        let cfg = srv.cfg();
        srv.read("desktop::history", |s| {
            match history_rows(s, &cfg, &id, 0, 0) {
                Ok(out) => (
                    StatusCode::OK,
                    Json(json!({
                        "messages": out,
                        // The cursor a client resumes from. Handed over with the
                        // transcript so it never has to derive one. Two cursors:
                        // messages AND notes (a note is anchored where it RENDERS;
                        // the note cursor says how far the note list itself was
                        // replayed — the twin the anchor cannot stand in for).
                        "last_msg_id": s.last_msg_id(&id).unwrap_or(0),
                        "last_note_id": s.last_note_id(&id).unwrap_or(0),
                    })),
                ),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("{e:#}") })),
                ),
            }
        })
    })
    .await
    .unwrap_or_else(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("reading the history: {e}") })),
        )
    })
}

/// POST /api/chat → 202 Accepted. Starts a turn and returns at once.
///
/// The turn runs detached, holding the per-session mutex, so two desktop
/// windows on one session serialize (busy→queue semantics, hermes
/// tui_gateway parity) instead of interleaving deltas. Its events ride the
/// session bus; the client watches them on `GET /api/sessions/:id/events`
/// — the SAME endpoint a second window or a reattach uses, so starting a
/// turn and viewing it are decoupled and there is no request-scoped stream
/// to lose. Cancellation is explicit (`POST /api/sessions/:id/cancel`,
/// riding the process-wide `agent::LIVE_TURNS`).
async fn chat(State(srv): State<Arc<DesktopServer>>, Json(req): Json<ChatRequest>) -> Response {
    if let Some(id) = req.join_id.as_deref().filter(|s| !s.is_empty()) {
        let Some(core) = crate::gateway::global_core_arc() else {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "join target is not held by this gateway"})),
            )
                .into_response();
        };
        return match core.desktop_command(id, &req.session_id, &req.text, &req.attachments) {
            Ok(Some(text)) => (
                StatusCode::OK,
                Json(json!({"handled": true, "join": true, "text": text})),
            )
                .into_response(),
            Ok(None) => (
                StatusCode::ACCEPTED,
                Json(json!({"started": true, "join": true})),
            )
                .into_response(),
            Err(e) => (
                StatusCode::CONFLICT,
                Json(json!({"error": format!("{e:#}")})),
            )
                .into_response(),
        };
    }
    // A session that lives on another node (it migrated, or this node
    // only holds a replica of it) is taken over here if its owner has
    // stopped answering and the arbiter allows it (lazy failover, C3);
    // otherwise the answer names where it lives instead of a turn the
    // fence would refuse.
    match veles_core::takeover::bring_here(srv.store.clone(), srv.cfg(), &req.session_id).await {
        Ok(veles_core::takeover::Brought::Here) => {}
        Ok(veles_core::takeover::Brought::TookOver(line)) => {
            tracing::warn!("failover: {line}");
            // A cached agent for the old tombstone would carry its stale
            // prompt; the next agent_for builds from the adopted rows.
            srv.agents.lock_ok().remove(&req.session_id);
        }
        Ok(veles_core::takeover::Brought::Elsewhere { node, name, why }) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "this session lives on node {name} — open it there ({why})"
                    ),
                    "owner": node,
                    "owner_name": name,
                })),
            )
                .into_response();
        }
        Err(e) => tracing::debug!("where does {} live? {e:#}", req.session_id),
    }
    let agent = match srv.agent_for(&req.session_id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{e:#}") })),
            )
                .into_response()
        }
    };
    let text = veles_core::media::with_attachments(&req.text, &req.attachments);

    // A slash line is a COMMAND, on the desktop as on every other
    // surface — never an LLM turn. `/api/chat` used to hand it to the
    // turn loop, so a module's direct command (`/atproto delete`,
    // `/slopdrop …`) posted from a desktop window or a script reached
    // the model as a prompt while Telegram/IRC/the TUI answered it
    // (BACKPORT DISCIPLINE, 2026-10-20: a delete had to be run by hand
    // against the live gateway, and only the messaging adapters could
    // spell it). The desktop is a LOCAL owner surface: the principal is
    // None, which `run_direct` reads as the owner. The command runs
    // detached like a turn (fire-and-forget): the user's line and the
    // reply are appended to the store and ride the session bus, so a
    // watching window renders them like any settled exchange.
    {
        let trimmed = text.trim_start();
        if trimmed.starts_with('/') {
            let sid = req.session_id.clone();
            let cmdline = trimmed.to_string();
            veles_core::rt::spawn(async move {
                let (head, rest) = match cmdline.split_once(' ') {
                    Some((h, r)) => (h, r.trim()),
                    None => (cmdline.as_str(), ""),
                };
                let mut cx = crate::commands::CommandCtx {
                    agent: &agent,
                    surface: crate::commands::Surface::Desktop,
                    history: None,
                    turn_live: false,
                    principal: None,
                };
                let out = crate::commands::dispatch_async(&mut cx, head, rest)
                    .await
                    .map(|h| h.text)
                    .unwrap_or_else(|| format!("unknown command {head}"));
                {
                    let store = agent.store.lock_ok();
                    let _ = store
                        .append_message(&sid, &veles_core::providers::ChatMessage::user(&cmdline));
                    let _ = store.append_message(
                        &sid,
                        &veles_core::providers::ChatMessage::assistant(&out, None),
                    );
                }
                veles_core::session_tap::publish(&sid, "user", json!({ "text": cmdline }));
                veles_core::session_tap::publish(&sid, "assistant", json!({ "text": out }));
            });
            return (
                StatusCode::ACCEPTED,
                Json(json!({"session_id": req.session_id, "started": true, "command": true})),
            )
                .into_response();
        }
    }

    // Auto-title: untitled sessions take a PROVISIONAL name immediately
    // (first line, truncated) so the sidebar is never raw ids, then an
    // async task upgrades it to a real LLM summary of the first prompt
    // (titles.rs). The task only overwrites a title that still equals
    // the provisional one, so a user rename always wins.
    {
        let sid = &req.session_id;
        let untitled = {
            let store = srv.store.lock_ok();
            matches!(store.title(sid), Ok(Some(t)) if t.trim().is_empty())
                || matches!(store.title(sid), Ok(None))
        };
        if untitled {
            // Title from the words, not the markers: an
            // attachment-only message would otherwise be titled
            // "MEDIA:/home/…/screenshot.png".
            let (_, words) = veles_core::media::extract_media(&text);
            let t: String = words
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(48)
                .collect();
            let provisional = t.trim().to_string();
            if !provisional.is_empty() {
                let _ = srv.store.lock_ok().set_title(sid, &provisional);
                let cfg = srv.cfg();
                let store = srv.store.clone();
                let sid_owned = sid.clone();
                let prompt = text.clone();
                veles_core::rt::spawn(async move {
                    if let Some(sum) = veles_core::titles::summarize_title(&cfg, &prompt).await {
                        let st = store.lock_ok();
                        // Don't clobber a rename or a newer provisional.
                        if matches!(st.title(&sid_owned), Ok(Some(cur)) if cur == provisional) {
                            let _ = st.set_title(&sid_owned, &sum);
                            tracing::info!("session {sid_owned}: titled '{sum}'");
                        }
                    }
                });
            }
        }
    }

    // FIRE-AND-FORGET. The turn runs detached; its events ride the
    // session bus (`veles_core::session_tap`), which `run_turn` feeds from
    // EVERY surface. Viewing them is `GET /api/sessions/:id/events`, which
    // the starting window attaches to exactly like a second window or a
    // reattach — so first-attach and resume are ONE path, which is the
    // whole point of the cutover.
    //
    // This used to build a `tokio::sync::mpsc` per request and return it
    // as the SSE body. mpsc is single-consumer, so the request that
    // started the turn was the only thing that could ever observe it: a
    // second window saw nothing, a reconnect could not rejoin, and once
    // that one reader was gone every send was discarded and the turn ran
    // blind. `session_retry` could not start a turn at all because it had
    // no way to wire up that request-scoped stream. The bus replaces all
    // of it, and `POST /api/chat` is now just "start a turn".
    let sid = req.session_id.clone();
    // Cancellation rides the ONE process-wide registry — `agent::LIVE_TURNS`,
    // which `run_turn` populates for EVERY surface and clears from its
    // `TurnGuard`. The token is created here only to hand to the turn.
    let cancel_for_task = CancelToken::new();
    // ── the queue (R4.6) ──
    // A message that arrives while a turn is live is NOT a second
    // concurrent turn and NOT a dropped keystroke: it is a queue row,
    // run at the turn boundary by whichever surface owns that boundary.
    // The live check is the store's session_runs (the same fact
    // /api/sessions shows as `running`), not this process's idea of
    // busy — a TUI turn queues a desktop message just like the
    // desktop's own.
    {
        let live = srv
            .store
            .lock_ok()
            .runs_live(30)
            .unwrap_or_default()
            .iter()
            .any(|r| r.session_id == sid);
        if live {
            let pushed = srv.store.lock_ok().queue_push(&sid, &text);
            match pushed {
                Ok(seq) => {
                    publish_queue_changed(&srv, &sid);
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({ "session_id": req.session_id, "started": false, "queued": seq })),
                    )
                        .into_response();
                }
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("{e:#}") })),
                    )
                        .into_response()
                }
            }
        }
    }
    let turn_lock = srv.turn_lock(&sid);
    // The window's language (X-Veles-Locale) does not ride into a
    // spawned task on its own; carry it so the turn's engine text
    // follows the desktop (the session's own language still wins).
    let ui_locale = veles_core::i18n::scoped();
    veles_core::rt::spawn(async move {
        // Turns on ONE session serialize here. This used to be a side
        // effect of holding the agent mutex; making it explicit is what
        // let the agent become shared, so a steer or a status read no
        // longer queues behind the turn it is asking about.
        let _turn = turn_lock.lock().await;
        // No asker install here: the desktop's asker is a session
        // ATTACH made once in `agent_for`. The old install/clear cycle
        // around this spawn was the asker-slot bug — a second
        // lifecycle's `clear_asker` (or this one's, racing a retry or a
        // wake drain) erased the registry entry the gateway's standing
        // ChatAsker depended on, and `clarify` answered "no user" while
        // the owner watched (PENDING 2026-09-14).
        // The drain loop (R4.6): one turn per queued message, in seq
        // order, reloading history between iterations so a turn that
        // ANOTHER surface ran while this one streamed is visible in the
        // next. `queue_take_next` promotes a pending row to `running`
        // ahead of the turn — the run row the turn then writes is what
        // keeps it honest, and a crash between those two writes is
        // demoted again by the next taker.
        let mut body = text;
        // Which queue row (if any) THIS loop is running. The first turn
        // answers the POST itself and owns no row; every later turn owns
        // exactly the seq the take handed us. Releasing only our own seq
        // keeps a row another surface holds alive for that surface.
        let mut cur_seq: Option<i64> = None;
        loop {
            let mut history = agent
                .store
                .lock()
                .unwrap()
                .load_history(&agent.session_id)
                .unwrap_or_default();
            // `run_turn` publishes everything observable about the turn
            // to the bus itself — stream events from its wrapper, the
            // verdict (`turn-result`/`turn-error`) when it returns — so
            // this spawn is the same three lines for every surface. The
            // only local concern is the asker, which must outlive the
            // request that started the turn (approvals park in the
            // registry).
            let _ = veles_core::i18n::scope(
                ui_locale.clone(),
                agent.run_turn(&mut history, &body, Some(cancel_for_task.clone()), |_| {}),
            )
            .await;
            // The turn we just ran is DONE — retire its row BEFORE the
            // next take. `turn_ended` already cleared our session_runs
            // row, so a completed `running` row left standing is
            // indistinguishable from a crashed taker's corpse, and the
            // recovery demotion would re-run the same message forever
            // (the 2026-09-08 ~100-turn flood).
            if let Some(seq) = cur_seq.take() {
                let _ = agent.store.lock().unwrap().queue_release(&sid, seq);
            }
            let taken = agent.store.lock().unwrap().queue_take_next(&sid);
            match taken {
                Ok(Some(item)) => {
                    body = item.body;
                    cur_seq = Some(item.seq);
                    publish_queue_changed(&srv, &sid);
                    continue;
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("queue drain failed for {sid}: {e:#}");
                    break;
                }
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "session_id": req.session_id, "started": true })),
    )
        .into_response()
}

// ── Parity endpoints (M5b): status, config, search, skills, memory,
// plugins, cron, usage, session title/override, desktop state blob.
// All read-only except title/override/cron-pause-resume-run/state, which
// mutate veles-owned state only.

use axum::extract::Query;
use std::collections::HashMap as StdMap;

/// GET /api/status — liveness + what's attached (hermes status-stack data).
async fn status(
    State(srv): State<Arc<DesktopServer>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let sessions = srv
        .read("desktop::status", |s| s.session_count())
        .unwrap_or(0);
    // Plan mode is PER SESSION, so it can only be answered for a named
    // one. `?session=<id>` is the same optional-scope shape /api/usage
    // already uses; without it the field is null rather than a guess,
    // because "false" would be a lie about a session we were not asked
    // about.
    let plan_mode = q.get("session").map(|sid| {
        srv.read("desktop::status", |s| {
            veles_core::plan::mode_for(s, sid) == veles_core::plan::Mode::Plan
        })
    });
    Json(json!({
        "status": "ok",
        "uptime_s": srv.started.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
        "session_count": sessions,
        "platforms": srv.bus_platforms(),
        "plan_mode": plan_mode,
        // Config changes a peer sent that wait for the owner (a security
        // section, or a conflict): the status line says so.
        "configsync_waiting": veles_core::configsync::inbox_waiting()
            .map(|w| w.iter().map(|(_, keys)| keys.len()).sum::<usize>())
            .unwrap_or(0),
    }))
}

/// GET /api/sessions/:id/context — how full the context window is.
///
/// USER: "add stats about the context window". `Agent::context_report`
/// already computed all of it (window size, compaction budget, how much
/// history is in it) for the `/context` command; nothing exposed it to
/// the desktop, so the one number that predicts a compaction was
/// invisible until it happened.
async fn context_view(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let agent = match srv.agent_for(&id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    // The whole history is read to measure it: off the async workers, on
    // a pooled connection, so a long session neither waits for the store
    // lock nor holds it.
    let hist_bytes = {
        let (srv, id) = (srv.clone(), id.clone());
        veles_core::rt::spawn_blocking(move || {
            srv.read("desktop::context", |s| {
                s.load_history(&id)
                    .map(|h| h.iter().map(|m| m.content.len()).sum::<usize>())
                    .ok()
            })
        })
        .await
        .ok()
        .flatten()
    };
    // Liveness comes from the process-wide registry, NOT from whether
    // this handler could lock its own Agent.
    //
    // `try_lock` failing means "the instance I hold is busy", which is a
    // different question. There are THREE Agent caches in this process
    // (this one, and two in gateway.rs), so a turn started from IRC,
    // telegram or cron ran against a different instance and this endpoint
    // reported the session idle while it was mid-turn. `turn_is_live` is
    // keyed by session id and registered inside `run_turn` itself, so it
    // answers for every surface.
    // ONE lookup answers both "is it busy" and "whose turn is it": the
    // snapshot is registered and cleared at the same place the cancel
    // token is (`run_turn`'s TurnGuard), so asking it twice could only
    // introduce a way for the two answers to disagree.
    let snap = veles_core::session_tap::snapshot(&id);
    // No `try_lock` fallback any more. The agent is shared, so the
    // report is always real; and this comment's own worry — "asking
    // twice could only introduce a way for the two answers to
    // disagree" — is now structurally impossible, because `busy` has
    // exactly one source.
    let mut report = agent.context_report(hist_bytes);
    report["busy"] = json!(snap.is_some());
    // Which surface owns the running turn, when one does. A desktop
    // showing "busy" with no idea whether that is its own turn or a cron
    // job is the state that had people closing the window on a turn they
    // wanted to keep.
    if let Some(snap) = snap {
        report["turn_surface"] = json!(snap.surface);
        report["turn_started_at"] = json!(snap.started_at);
    }
    // The resume cursor, so the caller never derives its own.
    report["last_msg_id"] = json!(srv
        .read("desktop::context", |s| s.last_msg_id(&id))
        .unwrap_or(0));
    (StatusCode::OK, Json(report))
}

/// GET /api/guards — harness-guard config + live per-session state.
///
/// The per-agent state is behind a tokio Mutex; `blocking_lock()` here
/// panicked ("Cannot block the current thread from within a runtime")
/// whenever a turn held the agent — the Aug 22 incident that poisoned
/// the store mutex behind it. Snapshot via try_lock instead: an agent
/// mid-turn is simply reported as "busy" rather than crashing the
/// endpoint (and, before lock_ok, every other endpoint with it).
#[derive(Deserialize)]
struct IrcSendBody {
    network: String,
    #[serde(default = "default_presence")]
    presence: String,
    target: String,
    text: String,
    /// "privmsg" (default), "action" or "notice".
    #[serde(default)]
    kind: Option<String>,
}

/// Say something as one of the connected presences.
///
/// The text goes out VERBATIM — whatever the composer held, control
/// bytes and all. The desktop deliberately does not convert markdown to
/// IRC formatting: rendering mIRC codes as markup on the way IN is
/// right, but doing the reverse to a human's typing would mangle any
/// message that merely contains an asterisk.
///
/// A failure is reported, never swallowed. A message that silently goes
/// nowhere is the worst outcome available here.
async fn irc_send(Json(body): Json<IrcSendBody>) -> (StatusCode, Json<Value>) {
    let kind = match body.kind.as_deref() {
        Some("action") => veles_core::irc_tap::SendKind::Action,
        Some("notice") => veles_core::irc_tap::SendKind::Notice,
        _ => veles_core::irc_tap::SendKind::Privmsg,
    };
    if body.target.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "no target"})));
    }
    match veles_core::irc_tap::send(veles_core::irc_tap::SendRequest {
        network: body.network,
        presence: body.presence,
        target: body.target,
        text: body.text,
        kind,
    }) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e}))),
    }
}

#[derive(Deserialize)]
struct IrcTypingBody {
    network: String,
    #[serde(default = "default_presence")]
    presence: String,
    target: String,
    /// "active" while composing, "done" on send or idle timeout.
    state: String,
}

/// Outbound typing indicator (typing cap). Fire-and-forget by design:
/// a refused indicator must never surface as an error in the composer,
/// because the composer's real job — the message — is unaffected. The
/// engine silently drops it when the connection never negotiated the
/// cap.
async fn irc_typing(Json(body): Json<IrcTypingBody>) -> StatusCode {
    veles_core::irc_tap::request_op(
        &body.network,
        &body.presence,
        veles_core::irc_tap::IrcOp::Typing {
            target: body.target,
            state: body.state,
        },
    )
    .await
    .map(|_| ())
    .unwrap_or(());
    // Always 200. An indicator the connection could not deliver changes
    // nothing the reader of this response can act on, and a composer
    // that toasts on every keystroke against a lagging gateway is worse
    // than no indicator at all.
    StatusCode::OK
}

#[derive(Deserialize)]
struct IrcOpBody {
    network: String,
    #[serde(default = "default_presence")]
    presence: String,
    #[serde(flatten)]
    op: veles_core::irc_tap::IrcOp,
}

/// Drive a connection: join, part, topic, kick, mode, whois.
///
/// The reply is whatever the server said, relayed verbatim. An op that
/// silently did nothing is indistinguishable from one the server
/// rejected, and "did my kick work" is not a question to leave someone
/// guessing about.
///
/// Confirmation for destructive ops is the DESKTOP's job, not this
/// endpoint's. The approvals layer (veles_core::ask) exists for the AGENT to
/// ask a human, keyed on a session and delivered through a turn's
/// stream; a human-initiated kick has neither, and routing it through
/// would mean inventing a session for a turn that does not exist.
async fn irc_op(Json(body): Json<IrcOpBody>) -> (StatusCode, Json<Value>) {
    match veles_core::irc_tap::request_op(&body.network, &body.presence, body.op).await {
        Ok(result) => (StatusCode::OK, Json(json!({"result": result}))),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e}))),
    }
}

// ── ACL management (the UI half of acl.rs) ───────────────────────────
//
// The desktop is an owner surface by acl.rs's own definition — the
// human at the keyboard IS the owner (full caps by definition). The
// bearer-token gate on this router is what separates it from the world
// (on loopback with no token configured, that gate is "nobody else can
// reach the port"). The read path returns the live grant table, the
// config pins, and the catalogs a UI renders; the mutation paths ride
// pairing::grant / pairing::revoke, which persist in the store (meta)
// and resolve through exactly the chain the gateway enforces (config
// pin > runtime grant > default role).
async fn acl_view(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let loaded = srv.cfg();
    let cfg = &loaded.acl;
    let mut identities: Vec<Value> = Vec::new();
    // Config pins first — they outrank grants in resolve().
    let mut pins: Vec<(&String, &String)> = cfg.roles.iter().collect();
    pins.sort();
    for (id, role) in pins {
        let p = veles_core::acl::Principal::resolve(Some(id), cfg);
        identities.push(json!({
            "identity": id,
            "source": "config",
            "role": role,
            "effective_role": p.role.as_str(),
            "caps": p.caps.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        }));
    }
    // Scoped config entries ([[acl.entries]]) — mask/channel/network
    // rules the UI must show next to the exact pins.
    for e in &cfg.entries {
        identities.push(json!({
            "identity": e.identity,
            "source": "entry",
            "role": e.role,
            "network": e.network,
            "channel": e.channel,
            "effective_role": e.role,
            "caps": [],
        }));
    }
    // Then runtime grants (pairing codes, desktop or IRC-tab grants).
    for g in veles_core::pairing::list_grants(&srv.store) {
        let p = veles_core::acl::Principal::resolve(Some(&g.identity), cfg);
        identities.push(json!({
            "identity": g.identity,
            "source": "grant",
            "role": g.role,
            "network": g.network,
            "channel": g.channel,
            "expires_at": if g.expires_at == 0 { Value::Null } else { json!(g.expires_at) },
            "granted_at": g.granted_at,
            "effective_role": p.role.as_str(),
            "caps": p.caps.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        }));
    }
    // Cross-network users: config-declared plus runtime records.
    let mut users: Vec<Value> = Vec::new();
    for u in &cfg.users {
        users.push(json!({
            "name": u.name,
            "role": u.role,
            "identities": u.identities,
            "source": "config",
        }));
    }
    for u in veles_core::pairing::list_users(&srv.store) {
        users.push(json!({
            "name": u.name,
            "role": u.role,
            "identities": u.identities,
            "source": "grant",
        }));
    }
    let cat = |v: Vec<(String, String)>| -> Vec<Value> {
        v.into_iter()
            .map(|(name, desc)| json!({"name": name, "desc": desc}))
            .collect()
    };
    // The per-role context ceilings, with the runtime overrides laid
    // over config — the same view `/limits` answers with, so the
    // overlay cannot drift from a chat line.
    let limits_cfg = veles_core::acl::with_runtime_limits(cfg.clone(), &srv.store);
    let limits: Vec<Value> = [
        veles_core::acl::Role::Guest,
        veles_core::acl::Role::User,
        veles_core::acl::Role::Trusted,
        veles_core::acl::Role::Admin,
        veles_core::acl::Role::Owner,
    ]
    .iter()
    .map(|r| {
        json!({
            "role": r.as_str(),
            "max_context_tokens": limits_cfg.limits.for_role(*r),
            // The two spend budgets ride the same row: a pane that
            // showed only the context ceiling would let an operator
            // believe there was no cost brake at all.
            "max_tokens_per_day": limits_cfg.limits.tokens_per_day_for_role(*r),
            "max_turns_per_hour": limits_cfg.limits.turns_per_hour_for_role(*r),
        })
    })
    .collect();
    Json(json!({
        "configured": cfg.is_configured(),
        "owners": cfg.owner,
        "default_role": cfg.default_role,
        "role_catalog": cat(veles_core::acl::role_catalog()),
        "cap_catalog": cat(veles_core::acl::cap_catalog()),
        "identities": identities,
        "users": users,
        "limits": limits,
        "default_max_context_tokens": limits_cfg.limits.default_max_context_tokens,
        "default_max_tokens_per_day": limits_cfg.limits.default_max_tokens_per_day,
        "default_max_turns_per_hour": limits_cfg.limits.default_max_turns_per_hour,
        "max_concurrent_turns": limits_cfg.limits.max_concurrent_turns,
        "runtime_limits": veles_core::acl::runtime_limits(&srv.store)
            .into_iter()
            .map(|(k, v)| json!({"which": k, "max_context_tokens": v}))
            .collect::<Vec<_>>(),
        // Every runtime override, not just the context ones: an
        // operator looking at this pane to find out why someone is
        // being refused needs to see the override that is doing it.
        "runtime_budgets": veles_core::acl::runtime_budgets(&srv.store)
            .into_iter()
            .map(|(b, which, v)| json!({"budget": b.as_str(), "which": which, "value": v}))
            .collect::<Vec<_>>(),
        // The emergency brake belongs in the security pane, and the
        // desktop is one of the three surfaces that can LIFT one — so
        // an operator who engaged it from their phone finds it here.
        "lockdown": match veles_core::lockdown::current(Some(&srv.store)) {
            None => Value::Null,
            Some(l) => json!({
                "by": l.by,
                "at": if l.at == 0 { Value::Null } else { json!(l.at) },
                "reason": l.reason,
                "from_config": l.from_config,
                "text": veles_core::lockdown::status(Some(&srv.store)),
            }),
        },
    }))
}

#[derive(Debug, serde::Deserialize)]
struct LockdownBody {
    /// `"on"` or `"off"`.
    action: String,
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /api/defaults {"command": "model qwen@ali-team"}` — the
/// install's default routes, written by `/defaults` itself.
///
/// The desktop's "Save as default" and the models overlay's Default…
/// each wrote `model.provider` and `model.default` as two separate
/// `/api/config/set` calls, resolving aliases in Tcl — one of five
/// writers for `[model]` (2026-10-04 defaults review). This runs the
/// same command the REPL, TUI and chat surfaces run, as the local owner
/// (the bearer is the desktop's), so a route means the same thing from
/// every surface and lands in one write.
async fn defaults_run(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let rest = body["command"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let loaded = srv.cfg();
    let ctx = crate::commands::AclCtx {
        store: &srv.store,
        fallback: &loaded.acl,
        session_id: "",
        principal: None,
        sender: None,
    };
    let text = crate::commands::defaults_command(&ctx, &rest);
    Json(json!({ "ok": true, "text": text }))
}

/// `POST /api/lockdown` — engage or lift the brake.
///
/// The desktop is a LOCAL surface, which is the whole reason this route
/// exists: engaging is reachable from anywhere, and lifting is
/// reachable only from here, the TUI, the REPL or the CLI. It goes
/// through the same `lockdown_command` every other surface calls, with
/// `Surface::Desktop`, rather than touching `lockdown::release`
/// directly — one implementation of the asymmetry, not two.
async fn lockdown_set(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<LockdownBody>,
) -> Json<Value> {
    let rest = match body.reason.as_deref().map(str::trim) {
        Some(r) if !r.is_empty() => format!("{} {r}", body.action),
        _ => body.action.clone(),
    };
    // No Agent: the brake is a store row and a config key, and
    // building a session to flip it would be the wrong dependency.
    let loaded = srv.cfg();
    let ctx = crate::commands::AclCtx {
        store: &srv.store,
        fallback: &loaded.acl,
        session_id: "",
        principal: None,
        sender: None,
    };
    let text = crate::commands::lockdown_command(&ctx, crate::commands::Surface::Desktop, &rest);
    Json(json!({
        "ok": true,
        "text": text,
        "engaged": veles_core::lockdown::engaged(Some(&srv.store)),
    }))
}

#[derive(Debug, serde::Deserialize)]
struct AclExplainQuery {
    /// An exact `platform:principal`, or a `nick!ident@host` mask.
    identity: String,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

/// `GET /api/acl/explain?identity=…` — the whole comparison, winner
/// first.
///
/// The question specificity precedence creates: with the narrowest
/// matching rule winning regardless of declaration order, "why does
/// this person have this role" is no longer answerable by reading
/// config.toml top to bottom. The desktop's ACL overlay needs the same
/// answer `/acl explain` gives every other surface.
async fn acl_explain(
    State(srv): State<Arc<DesktopServer>>,
    Query(q): Query<AclExplainQuery>,
) -> Json<Value> {
    let loaded = srv.cfg();
    let cfg = veles_core::acl::with_runtime_limits(
        veles_core::acl::live_config_or(&loaded.acl),
        &srv.store,
    );
    let is_mask = veles_core::ircmask::looks_like_mask(&q.identity);
    let ctx = veles_core::acl::SenderContext {
        mask: is_mask.then(|| q.identity.clone()),
        network: q.network.clone(),
        channel: q.channel.clone(),
        platform: q
            .platform
            .clone()
            .or_else(|| is_mask.then(|| "irc".to_string())),
        chat_id: q.channel.clone(),
        direct: q.channel.is_none(),
        handle: None,
        post: None,
    };
    let id = (!is_mask).then_some(q.identity.as_str());
    let rules = veles_core::acl::Principal::explain(id, &ctx, &cfg);
    let p = veles_core::acl::Principal::resolve_ctx(id, &ctx, &cfg);
    Json(json!({
        "identity": q.identity,
        "role": p.role.as_str(),
        "label": p.label(),
        "subject": p.subject,
        "via": p.via,
        "caps": p.caps.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "max_context_tokens": p.max_context_tokens(&cfg),
        // Winner first. `describe()` is the same line every other
        // surface prints, so the overlay and `/acl explain` cannot say
        // different things about one rule.
        "rules": rules
            .iter()
            .enumerate()
            .map(|(i, r)| json!({
                "winner": i == 0,
                "role": r.role.as_str(),
                "identity": r.identity,
                "source": r.source.as_str(),
                "exact_id": r.exact_id,
                "network": r.network,
                "channel": r.channel,
                "platform": r.platform,
                "user": r.user_name,
                "describe": r.describe(),
            }))
            .collect::<Vec<_>>(),
    }))
}

#[derive(Debug, serde::Deserialize)]
struct AclLimitBody {
    /// A role name, or `default`.
    which: String,
    /// Which budget. Absent = the context ceiling, so a client written
    /// before the other two existed keeps working unchanged.
    #[serde(default)]
    budget: Option<String>,
    /// 0 = unlimited. Absent = clear the runtime override.
    ///
    /// `max_context_tokens` is the name this field shipped with; it now
    /// carries whichever budget `budget` names. Kept rather than
    /// renamed because renaming it would break the desktop's own
    /// request shape for no gain.
    #[serde(default)]
    max_context_tokens: Option<u64>,
}

/// `POST /api/acl/limits` — set or clear one budget, live.
async fn acl_limit_set(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<AclLimitBody>,
) -> (StatusCode, Json<Value>) {
    let Some(budget) = veles_core::acl::Budget::parse(body.budget.as_deref().unwrap_or("context"))
    else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!(
                "unknown budget {:?} — context|tokens-per-day|turns-per-hour|concurrent-turns",
                body.budget.unwrap_or_default()
            )})),
        );
    };
    match body.max_context_tokens {
        Some(n) => match veles_core::acl::set_runtime_budget(&srv.store, budget, &body.which, n) {
            Ok(()) => (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "which": body.which,
                    "budget": budget.as_str(),
                    "max_context_tokens": n,
                    "value": n,
                })),
            ),
            Err(e) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": format!("{e:#}")})),
            ),
        },
        None => {
            let cleared = veles_core::acl::clear_runtime_budget(&srv.store, budget, &body.which);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "cleared": cleared,
                    "which": body.which,
                    "budget": budget.as_str(),
                })),
            )
        }
    }
}

/// `GET /api/sandboxes` — every declared box, every assignment, and
/// what podman actually has.
///
/// The desktop had no `user_envs` reference at all: the one surface
/// with the room to show a table of containers and who lands in which
/// could not see them.
async fn sandboxes_view(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let store = Some(&srv.store);
    let defs: Vec<Value> = veles_core::sandboxes::defs(store)
        .into_iter()
        .map(|d| {
            json!({
                "name": d.name,
                "image": d.image,
                "memory_mb": d.memory_mb,
                "network": d.network,
                "note": d.note,
                "created_by": d.created_by,
                "created_at": d.created_at,
            })
        })
        .collect();
    let binds: Vec<Value> = veles_core::sandboxes::binds(store)
        .into_iter()
        .map(|b| {
            json!({
                "identity": b.identity,
                "network": b.network,
                "channel": b.channel,
                "platform": b.platform,
                "role": b.role,
                "sandbox": b.sandbox,
                "key": veles_core::sandboxes::bind_key(&b),
            })
        })
        .collect();
    Json(json!({
        "admin_default_zone": if veles_core::user_envs::admin_default_is_sandbox() {
            "sandbox"
        } else {
            "host"
        },
        // The other half of the posture: where a call with NO principal
        // runs. This IS the desktop, so an operator reading the pane is
        // reading their own default.
        "local_default_zone": if veles_core::user_envs::local_default_is_sandbox() {
            "sandbox"
        } else {
            "host"
        },
        "shared": veles_core::sandboxes::SHARED,
        // How many execs one box runs at once. A shared box is a
        // shared resource, and this is the number that will refuse
        // somebody's call — so it belongs on the pane, not only in the
        // refusal.
        "max_concurrent_exec": veles_core::user_envs::max_concurrent_exec(),
        "defs": defs,
        "binds": binds,
        "live": veles_core::user_envs::status_list().await,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct SandboxDefBody {
    name: String,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    memory_mb: Option<u64>,
    #[serde(default)]
    network: Option<bool>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/sandboxes/create` — declare (or redeclare) one.
async fn sandbox_create(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<SandboxDefBody>,
) -> (StatusCode, Json<Value>) {
    let def = veles_core::config::SandboxDef {
        name: body.name,
        image: body.image,
        memory_mb: body.memory_mb,
        network: body.network,
        note: body.note,
        created_by: None,
        created_at: None,
    };
    match veles_core::sandboxes::define(&srv.store, def, "desktop") {
        Ok(d) => (StatusCode::OK, Json(json!({"ok": true, "name": d.name}))),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("{e:#}")})),
        ),
    }
}

#[derive(Debug, serde::Deserialize)]
struct SandboxNameBody {
    name: String,
    /// purge only: also delete the workspace, which holds people's
    /// files. Deliberately a separate flag rather than the default.
    #[serde(default)]
    delete_work: bool,
}

/// `POST /api/sandboxes/drop` — forget a runtime declaration. The
/// container and its workspace survive; that is `purge`.
async fn sandbox_drop(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<SandboxNameBody>,
) -> Json<Value> {
    let dropped = veles_core::sandboxes::undefine(&srv.store, &body.name);
    Json(json!({"ok": true, "dropped": dropped, "name": body.name}))
}

/// `POST /api/sandboxes/purge` — remove the container.
async fn sandbox_purge(
    State(_srv): State<Arc<DesktopServer>>,
    Json(body): Json<SandboxNameBody>,
) -> (StatusCode, Json<Value>) {
    let name = veles_core::sandboxes::normalize(&body.name);
    let key = veles_core::user_envs::key_for(&name);
    match veles_core::user_envs::purge(&key, body.delete_work).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"ok": true, "name": name, "workspace_deleted": body.delete_work})),
        ),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("{e:#}")})),
        ),
    }
}

#[derive(Debug, serde::Deserialize)]
struct SandboxBindBody {
    sandbox: String,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

impl SandboxBindBody {
    fn into_bind(self) -> veles_core::config::SandboxBind {
        // Empty strings arrive from a form where a field was left
        // blank; they are "no selector", not "match the empty string".
        let opt = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
        veles_core::config::SandboxBind {
            identity: opt(self.identity),
            network: opt(self.network),
            channel: opt(self.channel),
            platform: opt(self.platform),
            role: opt(self.role),
            sandbox: self.sandbox,
        }
    }
}

/// `POST /api/sandboxes/assign` — "these callers, in this place, use
/// that box."
async fn sandbox_assign(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<SandboxBindBody>,
) -> (StatusCode, Json<Value>) {
    match veles_core::sandboxes::bind(&srv.store, body.into_bind()) {
        Ok(b) => (
            StatusCode::OK,
            Json(json!({"ok": true, "key": veles_core::sandboxes::bind_key(&b)})),
        ),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("{e:#}")})),
        ),
    }
}

/// `POST /api/sandboxes/unassign` — drop one by the same selectors.
async fn sandbox_unassign(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<SandboxBindBody>,
) -> Json<Value> {
    let b = body.into_bind();
    let removed = veles_core::sandboxes::unbind(&srv.store, &b);
    Json(json!({"ok": true, "removed": removed, "key": veles_core::sandboxes::bind_key(&b)}))
}

/// `GET /api/commands/builtin` — the engine's own slash-command
/// table. (`/api/commands` is taken by the CUSTOM commands from
/// `.veles/commands/` and the plugins, which are a different thing.)
///
/// The desktop kept THREE hand-maintained copies of it (`chat.tcl`'s
/// switch and its help literal, `complete.tcl`'s "mirror of chat.tcl",
/// `palette.tcl`'s row list) and they had already drifted: they listed
/// commands that do not exist and omitted `/queue`, `/context`,
/// `/secret`, `/guards`, `/sessions` and `/cron`. One table, served,
/// with each row's required capability so a surface can mark the admin
/// rows instead of pretending everything is available.
async fn commands_view() -> Json<Value> {
    // Descriptions are translated for the ACTIVE locale (the gateway's
    // process locale, installed from `tui.locale` at boot). The `desc`
    // field is what a palette renders; the desktop's own catalogs cover
    // the strings IT owns, and this table is the engine's.
    let rows: Vec<Value> =
        crate::commands::help_entries_localized(crate::commands::Surface::Desktop)
            .into_iter()
            .map(|(usage, desc)| {
                // `usage` is "/name [args]"; the cap is keyed on the name.
                let name = usage
                    .split_whitespace()
                    .next()
                    .unwrap_or(&usage)
                    .trim_start_matches('/');
                json!({
                    "usage": usage,
                    "name": format!("/{name}"),
                    "desc": desc,
                    "cap": veles_core::acl::builtin_cap(name).map(|c| c.as_str()),
                })
            })
            .collect();
    Json(json!({
        "commands": rows,
        "shared": crate::commands::SHARED_COMMANDS,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct RunCommandBody {
    session_id: String,
    /// The whole line, slash included: `/limits set guest 90000`.
    command: String,
}

/// `POST /api/command` — run one SHARED command for a session.
///
/// The desktop was the only surface that never called
/// `commands::dispatch`. Everything it supported, it reimplemented in
/// Tcl — which is why its `/help` could advertise `/queue`, `/context`
/// and `/limits` and then answer "unknown command: /queue". This is the
/// seam that makes "first-class support in all frontends" true for the
/// commands the desktop has no native pane for: the Tcl side tries this
/// before it declares a name unknown.
///
/// Owner surface: no principal, so the tiering inside a command gives
/// the owner's answer. The desktop API is loopback and already
/// authenticated as the operator.
async fn command_run(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<RunCommandBody>,
) -> (StatusCode, Json<Value>) {
    let agent = match srv.agent_for(&body.session_id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("{e:#}")})),
            )
        }
    };
    let line = body.command.trim().to_string();
    let (head, rest) = match line.split_once(' ') {
        Some((h, r)) => (h.to_string(), r.trim().to_string()),
        None => (line.clone(), String::new()),
    };
    if !head.starts_with('/') {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "not a command (no leading /)"})),
        );
    }
    // A CHAT alias — `/qwen-flash <prompt>`. It mints a session, so it
    // cannot be a shared-dispatcher arm (see `commands::media_alias`).
    //
    // The desktop's sessions are keyed by id in `srv.agents` rather than
    // rebound behind a handle, so there is nothing to swap here: mint,
    // point the fresh agent at the route, and hand the id back. The
    // caller switches to it — `session_id` in the response is what says
    // so, and `_try_shared_command` acts on it.
    //
    // The PROMPT is not run here. `/api/command` answers commands; turns
    // go through the chat path, which is what streams. So the reply names
    // the session and the route and the pane carries the text over,
    // rather than this handler growing a second way to run a turn.
    if let Some(al) = veles_core::aliases::resolve_command(&agent.cfg, &head) {
        if al.kind == veles_core::aliases::AliasKind::Chat {
            let sid = crate::chat::new_session_id();
            if let Err(e) = srv.store.lock_ok().create_session(&sid) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("{e:#}") })),
                );
            }
            let fresh = match srv.agent_for(&sid) {
                Ok(a) => a,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("{e:#}") })),
                    )
                }
            };
            let route = match fresh.set_model(&al.model_arg()) {
                Ok(m) => m,
                Err(e) => {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "handled": true,
                            "text": format!("new session {sid}, but the route did not resolve: {e:#}"),
                            "reload": false,
                            "session_id": sid,
                        })),
                    )
                }
            };
            if let Some(r) = &al.reasoning {
                fresh.set_reasoning(r);
            }
            let text = if rest.is_empty() {
                format!("(new session {sid} · {route})")
            } else {
                format!("(new session {sid} · {route})\n{rest}")
            };
            return (
                StatusCode::OK,
                Json(json!({
                    "handled": true,
                    "text": text,
                    "reload": false,
                    "session_id": sid,
                    "prompt": rest,
                })),
            );
        }
    }

    let mut ctx = crate::commands::CommandCtx {
        agent: &agent,
        surface: crate::commands::Surface::Desktop,
        history: None,
        turn_live: veles_core::agent::turn_is_live(&body.session_id),
        principal: None,
    };
    // The async arms FIRST. `dispatch` is sync and answers those by
    // blocking on a fresh runtime, which from this handler — already on
    // a runtime worker — panics with "Cannot start a runtime from
    // within a runtime" and aborts the process. This used to carve out
    // `/sandbox` alone, so `/tcl` still took the gateway down.
    if let Some(h) = crate::commands::dispatch_async(&mut ctx, &head, &rest).await {
        return (
            StatusCode::OK,
            Json(json!({"handled": true, "text": h.text, "reload": h.reload})),
        );
    }
    match crate::commands::dispatch(&mut ctx, &head, &rest) {
        Some(h) => (
            StatusCode::OK,
            Json(json!({"handled": true, "text": h.text, "reload": h.reload})),
        ),
        // NOT an error: the caller falls through to its own handling
        // (a custom command, then "unknown command").
        None => (
            StatusCode::OK,
            Json(json!({"handled": false, "text": "", "reload": false})),
        ),
    }
}

#[derive(Debug, serde::Deserialize)]
struct AclGrantBody {
    identity: String,
    role: String,
    #[serde(default)]
    ttl_hours: Option<u64>,
    /// Scope the grant to one IRC network / one channel. Absent = any.
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    channel: Option<String>,
}

async fn acl_grant(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<AclGrantBody>,
) -> (StatusCode, Json<Value>) {
    let ttl_secs = body.ttl_hours.map(|h| h.saturating_mul(3600));
    let network = body.network.filter(|s| !s.trim().is_empty());
    let channel = body.channel.filter(|s| !s.trim().is_empty());
    match veles_core::pairing::grant_scoped(
        &srv.store,
        &body.identity,
        &body.role,
        ttl_secs,
        network,
        channel,
    ) {
        Ok(g) => (
            StatusCode::OK,
            Json(json!({"ok": true, "identity": g.identity, "role": g.role})),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

#[derive(Debug, serde::Deserialize)]
struct AclRevokeBody {
    identity: String,
}

async fn acl_revoke(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<AclRevokeBody>,
) -> (StatusCode, Json<Value>) {
    let removed = veles_core::pairing::revoke(&srv.store, &body.identity);
    (
        StatusCode::OK,
        Json(json!({"ok": removed, "identity": body.identity})),
    )
}

#[derive(Debug, serde::Deserialize)]
struct AclUserBody {
    name: String,
    role: String,
    #[serde(default)]
    identities: Vec<String>,
}

/// Create or replace a runtime user (cross-network identity group).
/// Config users ([[acl.users]]) are read-only here, same rule as pins.
async fn acl_user_save(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<AclUserBody>,
) -> (StatusCode, Json<Value>) {
    match veles_core::pairing::user_save(&srv.store, &body.name, &body.role, &body.identities) {
        Ok(u) => (
            StatusCode::OK,
            Json(json!({"ok": true, "name": u.name, "role": u.role})),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

#[derive(Debug, serde::Deserialize)]
struct AclUserDeleteBody {
    name: String,
}

async fn acl_user_delete(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<AclUserDeleteBody>,
) -> (StatusCode, Json<Value>) {
    let removed = veles_core::pairing::user_delete(&srv.store, &body.name);
    (
        StatusCode::OK,
        Json(json!({"ok": removed, "name": body.name})),
    )
}

#[derive(Debug, serde::Deserialize)]
struct AclPromoteBody {
    /// Report the lines and write nothing. The pane asks this first and
    /// shows the answer before offering the write.
    #[serde(default)]
    dry_run: bool,
    /// Write grants that still have an expiry, dropping it.
    #[serde(default)]
    include_expiring: bool,
    /// Fold runtime users into `[[acl.users]]` as well as grants.
    #[serde(default)]
    users: bool,
    /// Drop the store copies the file already decides.
    #[serde(default)]
    prune: bool,
}

/// `POST /api/acl/promote` — make a live decision survive the store.
///
/// The overlay could grant and revoke, so the only thing it could not do
/// was the one an operator eventually wants: keep a rule that turned out
/// to be permanent. The path was to read `/acl export` and paste, which
/// is how a config file acquires a rule nobody can explain afterwards.
///
/// This is `acl_command`'s `promote`, not a second implementation of it:
/// the flags are the body, the report is the text, and the refusals (an
/// unconfigured `[acl]`, a name the file already uses for a different
/// rule) arrive verbatim rather than being re-derived here and drifting.
///
/// Owner surface — no principal, so the `Cap::All` check inside the
/// command answers as the owner, and the bearer token on this API is what
/// says the caller is the operator.
async fn acl_promote(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<AclPromoteBody>,
) -> Json<Value> {
    let mut flags: Vec<&str> = Vec::new();
    if body.dry_run {
        flags.push("--dry-run");
    }
    if body.include_expiring {
        flags.push("--include-expiring");
    }
    if body.users {
        flags.push("--users");
    }
    if body.prune {
        flags.push("--prune");
    }
    let rest = if flags.is_empty() {
        "promote".to_string()
    } else {
        format!("promote {}", flags.join(" "))
    };
    let loaded = srv.cfg();
    let ctx = crate::commands::AclCtx {
        store: &srv.store,
        fallback: &loaded.acl,
        session_id: "",
        principal: None,
        sender: None,
    };
    let text = crate::commands::acl_command(&ctx, &rest);
    // `wrote` is what the pane reloads on; a refusal or a dry run is a
    // report to read, not a change to paint.
    let wrote = text.starts_with("promoted ");
    let refused = text.starts_with("refusing") || text.starts_with("promote failed");
    Json(json!({
        "ok": !refused,
        "wrote": wrote,
        "dry_run": body.dry_run,
        "text": text,
        "error": refused.then_some(text.clone()),
    }))
}

async fn guards(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let loaded = srv.cfg();
    let cfg = &loaded.agent.guards;
    let agents = srv.agents.lock_ok();
    let mut sessions = Vec::new();
    for (sid, agent) in agents.iter() {
        // Busy comes from the turn registry, not from whether a lock
        // happens to be contended: `session_tap` is registered and
        // cleared by `run_turn`'s own guard, so it answers for every
        // surface and never mistakes "another reader" for "a turn".
        let state = if veles_core::session_tap::snapshot(sid).is_some() {
            json!("busy (turn in progress)")
        } else {
            agent.guards_snapshot()
        };
        sessions.push(json!({ "session": sid, "state": state }));
    }
    Json(json!({
        "status": "ok",
        "config": {
            "max_iterations": cfg.max_iterations,
            "guardrail_warn_at": cfg.guardrail_warn_at,
            "guardrail_halt_at": cfg.guardrail_halt_at,
            "verification_nudge": cfg.verification_nudge,
            "verification_max_attempts": cfg.verification_max_attempts,
        },
        "active_sessions": sessions,
    }))
}

// ── IRC ──────────────────────────────────────────────────────────────

/// Every connection's current shape, plus which buffers have history.
///
/// A client needs both: `state` is what the connection is doing NOW
/// (channels joined, rosters, topics, whether it is even up), while
/// `buffers` includes channels it is no longer in but whose scrollback
/// is still readable. Listing only live channels would make leaving a
/// channel look like losing its history.
async fn irc_state(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let states = veles_core::irc_tap::states();
    let (all_buffers, latest_id) = srv.read("desktop::irc", |store| {
        let buffers: Vec<_> = states
            .iter()
            .map(|st| {
                store
                    .irc_buffers(&st.network, &st.presence)
                    .unwrap_or_default()
            })
            .collect();
        (buffers, store.irc_latest_id().unwrap_or(0))
    });
    let mut out = Vec::new();
    for (st, buffers) in states.iter().zip(all_buffers) {
        out.push(json!({
            "network": st.network,
            "presence": st.presence,
            "nick": st.nick,
            "connected": st.connected,
            "channels": st.channels,
            "topics": st.topics,
            "rosters": st.rosters,
            "aways": st.aways,
            "buffers": buffers,
        }));
    }
    Json(json!({
        "networks": out,
        // Which presences the composer may send as. A pane that offers
        // a send box for a connection that cannot send is a pane that
        // loses messages.
        "sendable": veles_core::irc_tap::sendable(),
        // Where "now" is, so a client that wants only live events can
        // subscribe from here instead of replaying everything.
        "latest_id": latest_id,
    }))
}

#[derive(Deserialize)]
struct IrcBacklogQuery {
    network: String,
    #[serde(default = "default_presence")]
    presence: String,
    buffer: String,
    /// Page backwards from this id. Absent means the newest page.
    before: Option<i64>,
    #[serde(default = "default_backlog_limit")]
    limit: u32,
}

fn default_presence() -> String {
    veles_core::irc_tap::PRESENCE_BOT.to_string()
}
fn default_backlog_limit() -> u32 {
    200
}

async fn irc_backlog(
    State(srv): State<Arc<DesktopServer>>,
    Query(q): Query<IrcBacklogQuery>,
) -> Json<Value> {
    let lines = srv
        .read("desktop::irc", |s| {
            s.irc_backlog(
                &q.network,
                &q.presence,
                &q.buffer,
                q.before,
                q.limit.clamp(1, 1000),
            )
        })
        .unwrap_or_default();
    Json(json!({ "lines": lines }))
}

#[derive(Deserialize)]
struct IrcEventsQuery {
    /// Resume from here. Absent means "only what happens from now on".
    since: Option<i64>,
}

/// The live event stream.
///
/// THIS IS THE BOUNCER GUARANTEE, and it is one mechanism rather than
/// two: `since` replays out of the store before switching to the live
/// tap, so "the desktop was closed for six hours" and "the socket
/// dropped for six seconds" are answered identically. A client stores
/// the last id it rendered and hands it back; nothing else is needed
/// for it to have missed nothing.
///
/// The replayed rows carry their durable ids and the live ones do not
/// yet — a live event has not been written when it is broadcast. So
/// the client tracks `id` while replaying and stops advancing its
/// cursor once live; if the socket then drops, it resumes from the
/// last durable id it saw and the store fills the gap. That is why the
/// replay must come first and must be inclusive of nothing already

#[derive(Deserialize)]
struct SessionEventsQuery {
    /// Resume point: the `msg_id` the client already has. Omitted or 0
    /// replays the whole transcript.
    #[serde(default)]
    since: i64,
    /// The note-side resume point: the highest `engine_notes.id` this
    /// client has already rendered. The message cursor cannot stand in
    /// for it — a note anchors where it RENDERS, but its id is what
    /// says whether THIS client has seen it.
    #[serde(default, rename = "since_note")]
    since_note: i64,
}

/// The rounds of an in-flight turn that the STORE does not already have.
///
/// A client attaching mid-turn is handed two things: the persisted rows
/// after its cursor, and the snapshot of the turn in flight. The agent
/// persists each round AS IT COMPLETES (`push_persist`, right after the
/// ToolEnd that ends it) while the tap keeps every round of the turn —
/// so a replay from a pre-turn cursor carried the finished rounds and
/// the snapshot carried them all over again. The desktop painted both.
///
/// `base_msg_id` was put on the snapshot for precisely this — "a client
/// attaching mid-turn replays the store up to it and takes everything
/// after from the snapshot" (agent.rs) — and no client ever read it.
/// This is the one place that holds both halves, so honouring it here
/// settles it for every client at once.
///
/// The count is of `role = "tool"` rows after the turn's base: one per
/// round, because the loop emits exactly one ToolEnd and persists
/// exactly one result row per round (a `batch` envelope runs its items
/// quiet — one round, one row, by the same contract). Trimming by what
/// is PERSISTED, not by the client's `since`, is what makes it right for
/// a later reconnect too: rounds below the cursor are already on that
/// client's screen and no longer deleted out from under it.
fn unpersisted_rounds(
    store: &Store,
    id: &str,
    mut snap: veles_core::session_tap::TurnSnapshot,
) -> veles_core::session_tap::TurnSnapshot {
    let persisted = store
        .load_history_since(id, snap.base_msg_id)
        .map(|rows| rows.iter().filter(|m| m.role == "tool").count())
        .unwrap_or(0);
    let done = persisted.min(snap.rounds.len());
    snap.rounds.drain(..done);
    snap
}

/// How often an attached stream checks the STORE for rows a turn in
/// ANOTHER PROCESS wrote.
///
/// `session_tap` is a process-wide broadcast channel — "one bus for
/// every session", and one bus per PROCESS. A delegate or `/btw` child
/// is a separate `veles chat -q`, so for a child session this process
/// is not merely quiet, it is subscribed to the wrong bus: there is no
/// events table in the store and no relay. Without this tick an
/// attached child replayed once at attach and then sat still for the
/// whole turn, which is the "it stopped streaming and looked hung even
/// though there were new messages coming in" report (USER 2026-09-18).
///
/// One `SELECT MAX(msg_id)` per tick per attached stream. Overridable
/// because a test cannot wait 1.5s per assertion.
fn follow_interval() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("VELES_SESSION_FOLLOW_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(1500),
    )
}

/// How long a turn's snapshot goes unmentioned before the follow tick
/// stops treating it as a live turn that owns the transcript.
///
/// `turn_ended` is the only thing that clears a snapshot, so a turn
/// that panics, is killed, or belongs to a process the gateway lost
/// leaves one standing for the life of the process — and the tick
/// would then never send a gap for that session again. Which is the
/// "attached and it just stopped streaming" report, re-armed by its
/// own fix.
///
/// 30s, not one tick: a turn is legitimately silent through a long
/// tool call, and `session_tap::publish` restamps on EVERY event, so
/// anything actually alive — including a turn parked on an `ask` —
/// refuses to go stale. The cost of being wrong is small in one
/// direction (a duplicate `history` row the client reconciles) and
/// unbounded in the other (a window that never updates again).
fn snapshot_stale_after() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("VELES_SNAPSHOT_STALE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(30_000),
    )
}

/// `POST /api/session-events` — events from a turn running in ANOTHER
/// PROCESS, republished onto this gateway's bus.
///
/// `session_tap` is process-wide, and a delegate or `/btw` child is a
/// separate `veles chat -q`. So a child's deltas, reasoning and tool
/// rounds are published to a bus with no subscribers while every
/// attached window listens here. `veles_core::relay` is the other end: the
/// child POSTs its own `SessionEvent`s verbatim and this republishes
/// them, after which the SSE handler needs no knowledge of any of it —
/// a relayed delta and a local one are the same event on the same bus.
///
/// Two things this must NOT do:
///
///   - Echo the child's `seq`. It is monotonic within a process RUN,
///     so the child's counter starts at 1 and means nothing here;
///     going through `session_tap::publish` restamps it with this
///     bus's own, which is what subscribers order on.
///   - Republish `turn-started`/`turn-ended` as plain events. They
///     carry the live-snapshot lifecycle, so they route to
///     `turn_started`/`turn_ended`, which publish the event themselves
///     as part of registering (and deregistering) the turn. A window
///     attaching mid-turn is then handed a real snapshot, and the
///     staleness guard has a turn to keep fresh.
///
/// No loop is possible: only a process with `VELES_RELAY_TO` set
/// relays, and nothing sets it on the gateway.
async fn relay_ingest(
    State(_srv): State<Arc<DesktopServer>>,
    Json(events): Json<Vec<veles_core::session_tap::SessionEvent>>,
) -> StatusCode {
    for ev in events {
        match ev.kind.as_str() {
            "turn-started" => {
                let surface = ev.data["surface"].as_str().unwrap_or("subagent");
                let base = ev.data["base_msg_id"].as_i64().unwrap_or(0);
                let prompt = ev.data["prompt"].as_str().unwrap_or("");
                let prompt_msg_id = ev.data["prompt_msg_id"].as_i64().unwrap_or(0);
                // The child names itself (pid, host), so the turn's
                // registry row is the child's and its death ends the
                // mirror here: a child killed at its timeout sends no
                // `turn-ended` (2026-10-04).
                let owner = ev.data["pid"]
                    .as_u64()
                    .and_then(|p| u32::try_from(p).ok())
                    .zip(ev.data["host"].as_str().map(str::to_string));
                veles_core::session_tap::turn_started_full(
                    &ev.session_id,
                    surface,
                    base,
                    prompt_msg_id,
                    prompt,
                    owner,
                    ev.data["injection"].as_str(),
                );
            }
            "turn-ended" => veles_core::session_tap::turn_ended(&ev.session_id),
            _ => {
                veles_core::session_tap::publish(&ev.session_id, &ev.kind, ev.data);
            }
        }
    }
    StatusCode::NO_CONTENT
}

/// GET /api/sessions/:id/events?since=<msg_id> — attach to a session.
///
/// This is the endpoint the desktop was missing, and its absence is why
/// three separate complaints existed. Events used to be obtainable ONLY
/// as a side effect of `POST /api/chat`, down a single-consumer channel
/// owned by that request, so:
///
///   - a second window could not watch a turn;
///   - a client that reconnected could not rejoin one, and fell back to
///     polling a busy flag every 3s while painting nothing, giving up
///     after 200 ticks — "the session view is stuck on a very old
///     message";
///   - and `session_retry` could not start a turn at all, saying so in
///     its own refusal.
///
/// Attaching and re-attaching are now the SAME operation, which is the
/// point: there is no first-attach path left to diverge from a resume
/// path.
///
/// The three phases are `irc_events`' three phases, in the same order and
/// for the same reasons:
///
///   1. Subscribe BEFORE reading the store, so an event landing mid-replay
///      is picked up by the live half instead of falling between the two.
///   2. Replay the persisted transcript after `since`, then the snapshot
///      of the turn in flight — the part the store cannot have yet,
///      because it is not finished.
///   3. A `session-live` marker, then the live tail, with `session-lagged`
///      when the broadcast drops rather than silence.
async fn session_events(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Query(q): Query<SessionEventsQuery>,
) -> Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let mut rx = veles_core::session_tap::subscribe();
    // THE SNAPSHOT FIRST, then what it owes the store, then the replay.
    //
    // The order is the whole safety argument. A round that persists
    // between two of these reads has to land on the DUPLICATE side of
    // the race, never the lost side: taken in this order, a round
    // persisted after `unpersisted_rounds` counted and before
    // `history_rows` read is carried by both and painted twice; taken
    // the other way round it would be carried by neither.
    //
    // The store reads run off the async workers on a pooled connection,
    // in this same order, and each sees a state at least as new as the
    // one before it.
    let snap = veles_core::session_tap::snapshot(&id);
    let (snap, replay, cursor, note_cursor) = {
        let (srv, id, since, since_note) = (srv.clone(), id.clone(), q.since, q.since_note);
        veles_core::rt::spawn_blocking(move || {
            srv.read("desktop::events", |st| {
                let snap = snap.map(|s| unpersisted_rounds(st, &id, s));
                let replay = history_rows(st, &srv.cfg.read_ok(), &id, since, since_note)
                    .unwrap_or_default();
                let cursor = st.last_msg_id(&id).unwrap_or(0);
                let note_cursor = st.last_note_id(&id).unwrap_or(since_note);
                (snap, replay, cursor, note_cursor)
            })
        })
        .await
        .unwrap_or_else(|_| (None, Vec::new(), 0, q.since_note))
    };
    let sid = id.clone();
    // A window now shows this session: it is held here for session mail
    // until the stream closes. Mail that waited while nobody had it open
    // runs now — "it runs when the session is next opened".
    let watch = srv.watch(&id);
    let mail_waiting = srv.read("desktop::events", |st| {
        st.queue_items(&id).is_ok_and(|q| {
            q.iter()
                .any(|i| i.kind.as_deref() == Some("mail") && i.state == "pending")
        })
    });
    if mail_waiting && srv.holds_for_mail(&id) {
        veles_core::cron::SessionRunner::kick_queue(
            &DesktopSessionRunner { srv: srv.clone() },
            &id,
        );
    }

    let stream = async_stream::stream! {
        let _watch = watch;
        for row in replay {
            yield Ok(Event::default()
                .event("history")
                .data(row.to_string()));
        }
        // What the turn in flight has produced but not yet persisted.
        // Without this a client attaching mid-turn sees the transcript up
        // to the last FINISHED message and then nothing until the turn
        // ends, which is exactly the "stuck" report.
        if let Some(s) = snap {
            yield Ok(Event::default()
                .event("turn-snapshot")
                .data(serde_json::to_string(&s).unwrap_or_else(|_| "{}".into())));
        }
        // The past is done; everything after this is live.
        yield Ok(Event::default()
            .event("session-live")
            .data(json!({ "last_msg_id": cursor, "last_note_id": note_cursor }).to_string()));
        // The queue as it stands NOW — a client attaching mid-queue
        // needs the pending list without waiting for the next change.
        yield Ok(Event::default()
            .event("queue")
            .data(queue_items_json(&srv, &id).to_string()));
        // The live tail, with a RUN of same-kind text deltas drained into
        // one frame before it goes out.
        //
        // No timer, and that is the point. When the client keeps up,
        // `try_recv` finds an empty channel and every token leaves as its
        // own frame at full latency. When it falls behind — which is
        // exactly when a provider dumps a burst; stream_pacing measured
        // 766 SSE frames inside one 239ms window — the backlog is folded
        // on the way OUT instead of being written one tiny frame at a
        // time into a socket nobody is reading. Coalescing appears only
        // under the pressure that calls for it, it raises the drain rate,
        // and raising the drain rate is what stops the lag → reconnect →
        // whole-session-reload loop at its source.
        //
        // `pending` holds the one event that ended a run: it belongs to
        // the next iteration, not to this frame, and dropping it would
        // lose a tool round.
        let mut pending: Option<veles_core::session_tap::SessionEvent> = None;
        let mut lagged: Option<u64> = None;
        let mut closed = false;
        // THE STORE-FOLLOW CURSOR. `cursor` above is where the replay
        // left the client; from here it tracks what this stream has
        // accounted for, by either half — the bus or the tick below.
        let mut cursor = cursor;
        // The note-side twin. A note can be created with NO new message
        // behind it (its anchor is the newest message AT CREATION, not
        // a message that arrived with it), so "messages advanced" is
        // not the same fact as "notes advanced". The tick below checks
        // both heads: a note-only gap sends exactly the notes whose ids
        // are past this cursor.
        let mut note_cursor = note_cursor;
        // The notes THIS STREAM has already delivered live, by id. The
        // note cursor above stays behind them on purpose, so the quiet
        // tick after a turn finds them in its gap — and sending them a
        // second time painted every note of the turn again, in one heap
        // under the answer (USER 2026-09-30: "the guards are still dumped
        // again at the end of the agent turn all at once"). This stream
        // knows exactly what it sent; the gap leaves those out and keeps
        // everything else, which is what the cursor was held back for.
        let mut bus_notes: std::collections::HashSet<i64> = std::collections::HashSet::new();
        // Has the bus said anything since the last tick? Every event on
        // it is from THIS process, so "nothing at all since the last
        // tick" is how a stream tells that rows appearing in the store
        // were written by somebody else.
        let mut bus_quiet = true;
        loop {
            if let Some(n) = lagged.take() {
                // Say so rather than pretending. The client refetches
                // from `last_msg_id`, which the store still has.
                yield Ok(Event::default()
                    .event("session-lagged")
                    .data(json!({ "missed": n }).to_string()));
            }
            if closed {
                break;
            }
            let mut ev = match pending.take() {
                Some(e) => e,
                None => {
                    // Wait for the bus OR the follow tick, whichever
                    // comes first. The bus is still the primary source
                    // and nothing about its latency changes: when it is
                    // talking, `recv` wins every race and the tick only
                    // ever resyncs a cursor.
                    let next = tokio::select! {
                        biased;
                        r = rx.recv() => Some(r),
                        _ = tokio::time::sleep(follow_interval()) => None,
                    };
                    let Some(r) = next else {
                        // A TICK. Who owns this session's transcript?
                        // The two head reads share one pooled connection,
                        // never the store lock: this ticks every 1.5 s in
                        // every open window, on a tokio worker.
                        let (head, notes_head) = srv.read("desktop::events", |st| {
                            (
                                st.last_msg_id(&sid).unwrap_or(cursor),
                                st.last_note_id(&sid).unwrap_or(note_cursor),
                            )
                        });
                        if !bus_quiet
                            || veles_core::session_tap::snapshot_fresh(&sid, snapshot_stale_after())
                                .is_some()
                        {
                            // A turn in THIS process is running, or just
                            // spoke. Its rows reach the client as live
                            // events and its turn-result reconciles them;
                            // re-sending them as history would paint the
                            // turn twice. Resync and say nothing.
                            //
                            // The snapshot check is not redundant with
                            // the bus check: a long silent tool call
                            // produces no events for many ticks while
                            // its result row lands in the store.
                            //
                            // The MESSAGE cursor resyncs unconditionally.
                            // The NOTE cursor deliberately does NOT: a
                            // note written by ANOTHER process while this
                            // turn is busy would be jumped over and never
                            // sent. Instead the note cursor stays until
                            // the next quiet gap. That gap also holds
                            // this turn's own bus-delivered notes, and
                            // the stream leaves those out by id
                            // (bus_notes).
                            cursor = head;
                        } else if head > cursor || notes_head > note_cursor {
                            // Rows with NO bus traffic behind them: the
                            // writer is another process, or a note was
                            // persisted with no new message behind it.
                            // Send exactly the gap, in the same `history`
                            // frames the attach replay uses, then move
                            // the client's cursors with
                            // `history-appended` — NOT with
                            // `session-live` (whose arm re-handshakes a
                            // stream it was handed no snapshot for, once
                            // per tick) and NOT with `history-changed`
                            // (whose arm repaints the whole transcript).
                            // Read the gap into a Vec BEFORE yielding:
                            // the config guard must not be held across
                            // an await, or the whole stream stops being
                            // Send and axum will not take it.
                            let gap = {
                                let cfg = srv.cfg.read_ok();
                                srv.read("desktop::events", |st| {
                                    history_rows(st, &cfg, &sid, cursor, note_cursor)
                                })
                                .unwrap_or_default()
                            };
                            for row in gap {
                                if row["role"] == "note"
                                    && row["note_id"].as_i64().is_some_and(|n| bus_notes.contains(&n))
                                {
                                    continue;
                                }
                                yield Ok(Event::default()
                                    .event("history")
                                    .data(row.to_string()));
                            }
                            cursor = head;
                            note_cursor = notes_head;
                            bus_notes.retain(|n| *n > note_cursor);
                            yield Ok(Event::default()
                                .event("history-appended")
                                .data(json!({ "last_msg_id": cursor, "last_note_id": note_cursor }).to_string()));
                        }
                        bus_quiet = true;
                        continue;
                    };
                    match r {
                        Ok(e) => {
                            // One bus for every session, filtered here — the
                            // same arrangement irc_tap uses, and the reason a
                            // session does not need a channel of its own to
                            // be reaped.
                            if e.session_id != sid {
                                continue;
                            }
                            bus_quiet = false;
                            e
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            lagged = Some(n);
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            };
            loop {
                match rx.try_recv() {
                    Ok(next) => {
                        if next.session_id != sid {
                            continue;
                        }
                        if !veles_core::session_tap::merge_delta(&mut ev, &next) {
                            pending = Some(next);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                        // Report it AFTER this frame: the text already
                        // merged is real and the client should have it
                        // before being told to refetch the gap.
                        lagged = Some(n);
                        break;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        closed = true;
                        break;
                    }
                }
            }
            if ev.kind == "note" {
                if let Some(n) = ev.data["id"].as_i64().filter(|n| *n > 0) {
                    bus_notes.insert(n);
                }
            }
            yield Ok(Event::default().event(&ev.kind).data(ev.data.to_string()));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
/// sent.
async fn irc_events(
    State(srv): State<Arc<DesktopServer>>,
    Query(q): Query<IrcEventsQuery>,
) -> Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let mut rx = veles_core::irc_tap::subscribe();
    // Take the backlog BEFORE draining the live channel, so a line
    // landing mid-replay is picked up by the live half rather than
    // falling between the two.
    let replay = match q.since {
        Some(since) => srv
            .read("desktop::irc", |s| s.irc_since(since, 5000))
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let stream = async_stream::stream! {
        for line in replay {
            yield Ok(Event::default()
                .event("irc")
                .data(serde_json::to_string(&line).unwrap_or_else(|_| "{}".into())));
        }
        // A marker so the client knows the past is done and what it is
        // seeing from here is live.
        yield Ok(Event::default().event("irc-live").data("{}"));
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    yield Ok(Event::default()
                        .event("irc")
                        .data(serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into())));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Say so rather than pretending. The client refetches
                    // from the store, which has the lines this stream
                    // just dropped.
                    yield Ok(Event::default()
                        .event("irc-lagged")
                        .data(json!({"missed": n}).to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── the phone's notifications ───────────────────────────────────────
//
// USER 2026-09-30, asked whether the phone should notify while the
// Veles window is in the background: "Asks and finished turns". The
// app's gateway service (a foreground service, which Android does not
// freeze the way it freezes the backgrounded desktop) reads this stream
// and posts Android notifications from it (android/java/…/Notifier.java).
//
// Three frames:
//   ask           an ask opened: id, session, kind, prompt, title. Sent
//                 for the asks already open when the stream opens, too.
//   ask-resolved  it was answered or dropped: its notification goes.
//   turn-done     a turn the DESKTOP or a phone TRIGGER started finished:
//                 session, title, and the first line of the answer.
// Asks of a turn IRC or Telegram started are theirs to notify, and they
// do; a turn a cron job or a chat started did not come from the person
// holding the phone.

/// The notifier's frame for one bus event, before the store is asked
/// for a title. `surface` is the running turn's, when there is one.
fn notify_frame(
    ev: &veles_core::session_tap::SessionEvent,
    surface: Option<&str>,
) -> Option<(&'static str, Value)> {
    match ev.kind.as_str() {
        "ask" => {
            // The ask carries its turn's surface (session_tap stamps it);
            // the running turn's is the fallback.
            let s = ev.data.get("surface").and_then(|v| v.as_str()).or(surface);
            if matches!(s, Some("irc" | "telegram")) {
                return None;
            }
            Some((
                "ask",
                json!({
                    "id": ev.data.get("id").cloned().unwrap_or(Value::Null),
                    "session_id": ev.session_id,
                    "kind": ev.data.get("kind").cloned().unwrap_or(Value::Null),
                    "prompt": ev.data.get("prompt").cloned().unwrap_or(Value::Null),
                }),
            ))
        }
        "ask-resolved" => Some((
            "ask-resolved",
            json!({
                "id": ev.data.get("id").cloned().unwrap_or(Value::Null),
                "session_id": ev.session_id,
            }),
        )),
        "turn-ended" => {
            // The desktop's turns, and a phone trigger's (its reply is
            // what the person set it up to hear).
            let s = ev.data.get("surface").and_then(|v| v.as_str()).or(surface);
            matches!(s, Some("desktop" | "trigger"))
                .then(|| ("turn-done", json!({ "session_id": ev.session_id })))
        }
        _ => None,
    }
}

/// The session's title, and the first line of its last answer (at most
/// 140 characters), for a notification's two lines.
fn notify_context(srv: &DesktopServer, session_id: &str) -> (String, String) {
    let store = srv.store.lock_ok();
    let title = store.title(session_id).ok().flatten().unwrap_or_default();
    let answer = store
        .recent_messages(session_id, 8)
        .map(|(msgs, _)| {
            msgs.iter()
                .rev()
                .find(|m| m.role == "assistant" && !m.content.trim().is_empty())
                .map(|m| {
                    let line = m.content.trim().lines().next().unwrap_or("").trim();
                    notify_plain(line).chars().take(140).collect::<String>()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    (title, answer)
}

/// One line of an answer as a notification shows it: a link as its
/// text, and no markdown marks (a phone's notification is plain text;
/// `[the docs](https://…)` read as what it is).
fn notify_plain(line: &str) -> String {
    static LINK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static MARKS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static LEAD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let link = LINK.get_or_init(|| regex::Regex::new(r"\[([^\]]+)\]\([^)]*\)").unwrap());
    let marks = MARKS.get_or_init(|| regex::Regex::new(r"\*\*|__|`").unwrap());
    let lead = LEAD.get_or_init(|| regex::Regex::new(r"^(#{1,6}\s+|[-*+>]\s+)").unwrap());
    let s = link.replace_all(line, "$1");
    let s = marks.replace_all(&s, "");
    lead.replace(&s, "").trim().to_string()
}

async fn notify_events(
    State(srv): State<Arc<DesktopServer>>,
) -> Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let mut rx = veles_core::session_tap::subscribe();
    // The asks open NOW, taken after subscribing so one opening in
    // between arrives twice rather than never (the notifier keys them by
    // id).
    let open = veles_core::ask::open_all();
    // Desktop turns that ended while NO notifier listened (its stream is
    // live-only, so without this a turn that finished during a reconnect
    // was never announced), each handed over once. The guard lives as long
    // as the stream: while it does, ends go out live and are not kept, so
    // a turn this notifier already saw — or skipped because the desktop
    // was in front — is never posted again by a later reconnect.
    let (listening, missed) = veles_core::session_tap::notify_listen();
    let stream = async_stream::stream! {
        let _listening = listening;
        for req in open {
            let surface = veles_core::session_tap::snapshot(&req.session_id).map(|s| s.surface);
            if matches!(surface.as_deref(), Some("irc" | "telegram")) {
                continue;
            }
            let (title, _) = notify_context(&srv, &req.session_id);
            yield Ok(Event::default().event("ask").data(
                json!({
                    "id": req.id, "session_id": req.session_id, "kind": req.kind,
                    "prompt": req.prompt, "title": title,
                })
                .to_string(),
            ));
        }
        for session_id in missed {
            // Skip a turn that is somehow live again (a new turn started):
            // the live stream will announce its end when it comes.
            if veles_core::session_tap::snapshot(&session_id).is_some() {
                continue;
            }
            let (title, answer) = notify_context(&srv, &session_id);
            yield Ok(Event::default().event("turn-done").data(
                json!({ "session_id": session_id, "title": title, "text": answer })
                    .to_string(),
            ));
        }
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let surface = veles_core::session_tap::snapshot(&ev.session_id).map(|s| s.surface);
                    let Some((name, mut data)) = notify_frame(&ev, surface.as_deref()) else {
                        continue;
                    };
                    if name != "ask-resolved" {
                        let (title, answer) = notify_context(&srv, &ev.session_id);
                        data["title"] = json!(title);
                        if name == "turn-done" {
                            data["text"] = json!(answer);
                        }
                    }
                    yield Ok(Event::default().event(name).data(data.to_string()));
                }
                // The notifier lost frames: it reconnects, and the open
                // asks are sent again.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── language server ─────────────────────────────────────────────────
//
// The gateway runs the language server, because the gateway is where the
// files are. See `veles_core::lsp_proxy` for what this is and — more usefully
// — what it deliberately is not: it does not parse a single JSON-RPC
// field. `lspclient.tcl` owns all of that and is tested on it.

#[derive(Deserialize)]
struct LspStart {
    /// A file being edited. The project root is derived from it HERE,
    /// because the filesystem it lives on is this one — the desktop
    /// cannot stat a remote path to look for a `.git`.
    #[serde(default)]
    path: Option<String>,
    /// An explicit root, when the caller knows better than the walk.
    #[serde(default)]
    root: Option<String>,
    /// Server command; defaults to tcl-lsp, resolved on the GATEWAY's
    /// PATH. That is the point: the desktop machine no longer needs it
    /// installed at all.
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

/// Walk up from a file to the directory that looks like its project.
///
/// `rootUri` was `null` before this, so no language server this app ever
/// ran had a project to index — which is why "go to definition in another
/// file" never worked, locally as much as remotely.
fn project_root_for(path: &std::path::Path) -> std::path::PathBuf {
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/"))
    };
    for dir in start.ancestors() {
        for marker in [".git", ".hg", ".jj", "Cargo.toml", "flake.nix"] {
            if dir.join(marker).exists() {
                return dir.to_path_buf();
            }
        }
    }
    start
}

async fn lsp_start(Json(body): Json<LspStart>) -> Json<Value> {
    let cmd = body.cmd.unwrap_or_else(|| "tcl-lsp".to_string());
    let (root, canonical) = match (&body.root, &body.path) {
        (Some(r), _) => (std::path::PathBuf::from(r), body.path.clone()),
        (None, Some(p)) => {
            let pb = std::path::Path::new(p)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(p));
            (
                project_root_for(&pb),
                Some(pb.to_string_lossy().to_string()),
            )
        }
        (None, None) => return Json(json!({"ok": false, "error": "need a path or a root"})),
    };
    let root_s = root.to_string_lossy().to_string();
    match veles_core::lsp_proxy::start(&cmd, &body.args, &root_s).await {
        Ok(()) => Json(json!({
            "ok": true, "cmd": cmd, "root": root_s, "path": canonical
        })),
        Err(e) => Json(json!({"ok": false, "error": e, "root": root_s})),
    }
}

/// POST /api/lsp/send — one JSON-RPC message, forwarded verbatim.
///
/// The body is the message itself, not a wrapper around it: the client
/// already builds LSP JSON and re-encoding it here would mean this file
/// had opinions about a protocol it is only carrying.
async fn lsp_send(body: String) -> Json<Value> {
    if body.trim().is_empty() {
        return Json(json!({"ok": false, "error": "empty body"}));
    }
    match veles_core::lsp_proxy::send(&body).await {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

async fn lsp_stop() -> Json<Value> {
    Json(json!({"ok": true, "was_running": veles_core::lsp_proxy::stop()}))
}

async fn lsp_status() -> Json<Value> {
    match veles_core::lsp_proxy::current() {
        Some((root, cmd)) => Json(json!({"running": true, "root": root, "cmd": cmd})),
        None => Json(json!({"running": false})),
    }
}

/// GET /api/lsp/events — every frame off the server's stdout, verbatim.
///
/// Responses AND notifications: only the client knows a
/// `publishDiagnostics` from an answer to something it asked.
///
/// No replay on attach, unlike the IRC stream. A response to a request
/// nobody is waiting for is noise, and a reattaching client re-runs
/// `initialize` anyway. `lsp-lagged` is still worth saying: a client that
/// silently missed a response would sit on a pending id forever.
async fn lsp_events(
) -> Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let mut rx = veles_core::lsp_proxy::subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("lsp-live").data("{}"));
        loop {
            match rx.recv().await {
                // Wrapped, not raw: axum splits a payload containing a
                // newline into several `data:` lines, and the client
                // reads one. serde_json escapes newlines, so the wrapper
                // is always a single line whatever the server emitted.
                Ok(frame) => yield Ok(Event::default()
                    .event("lsp")
                    .data(json!({"frame": frame}).to_string())),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    yield Ok(Event::default()
                        .event("lsp-lagged")
                        .data(json!({"missed": n}).to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// POST /api/tcl/eval — evaluate Tcl in the live gateway interpreter.
/// Body: {"code": "..."} → {"ok": bool, "result": "..."}.
/// Loopback-only; this is the REPL/debugger/hot-patch surface.
async fn tcl_eval(State(srv): State<Arc<DesktopServer>>, Json(body): Json<Value>) -> Json<Value> {
    let code = body["code"].as_str().unwrap_or("");
    if code.trim().is_empty() {
        return Json(json!({"ok": false, "result": "error: empty code"}));
    }
    match srv.hooks.eval_tcl(code).await {
        Ok(result) => Json(json!({"ok": true, "result": result})),
        Err(e) => Json(json!({"ok": false, "result": e})),
    }
}

/// How long to wait for the desktop to answer an app-target eval.
///
/// Long enough for a slow redraw, short enough that a wedged window
/// does not hold the agent's turn open. The desktop echoes the request
/// into its REPL transcript before running it, so a timeout is
/// diagnosable rather than mysterious.
const DESKTOP_EVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// POST /api/desktop/eval {code, target} — evaluate Tcl.
///
/// `target: "gw"` is the gateway's plugin interpreter, the same one
/// `/api/tcl/eval` serves. `target: "app"` is the DESKTOP WINDOW'S own
/// interpreter, which nothing could previously reach from here — the
/// request is handed to the connected desktop over SSE and its answer
/// comes back through `/api/desktop/eval/result`.
///
/// The app target is gated on the desktop having armed live patching.
/// Both refusals below are advisory mirrors of what the desktop tracks;
/// the desktop refuses on its own account too, so losing this state
/// cannot let an eval through.
async fn desktop_eval(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let code = body["code"].as_str().unwrap_or("").trim().to_string();
    if code.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "result": "empty code" })),
        );
    }
    let target = body["target"].as_str().unwrap_or("gw");
    if target == "gw" {
        return match srv.hooks.eval_tcl(&code).await {
            Ok(result) => (
                StatusCode::OK,
                Json(json!({ "ok": true, "target": "gw", "result": result })),
            ),
            Err(e) => (
                StatusCode::OK,
                Json(json!({ "ok": false, "target": "gw", "result": e })),
            ),
        };
    }
    if target != "app" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "result": format!("unknown target `{target}` (gw | app)") })),
        );
    }
    if !srv.desktop_eval.connected() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "ok": false,
                "result": "no desktop window is listening (start it with `veles gui`)",
            })),
        );
    }
    if !srv.desktop_eval.is_armed() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "result": "live patching is not armed — switch it on in the Dev pane first",
            })),
        );
    }

    let id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    srv.desktop_eval.pending.lock_ok().insert(id.clone(), tx);
    let sent = srv.desktop_eval.requests.send(json!({
        "id": id,
        "code": code,
        "note": body["note"].as_str().unwrap_or(""),
    }));
    if sent.is_err() {
        srv.desktop_eval.pending.lock_ok().remove(&id);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "result": "the desktop dropped its subscription" })),
        );
    }
    match tokio::time::timeout(DESKTOP_EVAL_TIMEOUT, rx).await {
        Ok(Ok(answer)) => (StatusCode::OK, Json(answer)),
        // The desktop went away mid-request.
        Ok(Err(_)) => {
            srv.desktop_eval.pending.lock_ok().remove(&id);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "ok": false, "result": "the desktop closed before answering" })),
            )
        }
        Err(_) => {
            srv.desktop_eval.pending.lock_ok().remove(&id);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({
                    "ok": false,
                    "result": format!(
                        "the desktop did not answer within {}s — the request is in its Dev REPL transcript",
                        DESKTOP_EVAL_TIMEOUT.as_secs()
                    ),
                })),
            )
        }
    }
}

/// GET /api/desktop/eval/events — the desktop's work queue.
async fn desktop_eval_events(
    State(srv): State<Arc<DesktopServer>>,
) -> Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let mut rx = srv.desktop_eval.requests.subscribe();
    let stream = async_stream::stream! {
        // Say hello immediately: the client needs to know the
        // subscription is live before it advertises itself as armed.
        yield Ok(Event::default().event("eval-live").data("{}"));
        loop {
            match rx.recv().await {
                Ok(req) => yield Ok(Event::default().event("eval").data(req.to_string())),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    yield Ok(Event::default()
                        .event("eval-lagged")
                        .data(json!({ "missed": n }).to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct DeviceEventsQuery {
    client_id: String,
}

/// Detaches a device client when its stream closes, however it closes.
struct DeviceAttach(String);

impl Drop for DeviceAttach {
    fn drop(&mut self) {
        veles_core::devices::detach(&self.0);
    }
}

/// GET /api/device/events?client_id= — a window's request stream. The
/// first event says the stream is live (the window then sends its
/// catalog); every call for this client follows as a `request`.
async fn device_events(
    Query(q): Query<DeviceEventsQuery>,
) -> Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let mut rx = veles_core::devices::attach(&q.client_id);
    let guard = DeviceAttach(q.client_id);
    let stream = async_stream::stream! {
        let _guard = guard;
        yield Ok(Event::default().event("device-live").data("{}"));
        while let Some(req) = rx.recv().await {
            yield Ok(Event::default()
                .event("request")
                .data(serde_json::to_string(&req).unwrap_or_else(|_| "{}".into())));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// POST /api/device/hello {client_id, name, platform, capabilities}.
async fn device_hello(Json(h): Json<veles_core::devices::Hello>) -> (StatusCode, Json<Value>) {
    if h.client_id.trim().is_empty() || h.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a device needs a client_id and a name" })),
        );
    }
    let n = h.capabilities.len();
    tracing::info!("device {} ({}) offers {n} action(s)", h.name, h.platform);
    veles_core::devices::hello(h);
    (StatusCode::OK, Json(json!({ "ok": true, "actions": n })))
}

/// POST /api/device/result {id, ok, result} — a window's answer.
async fn device_result(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let Some(id) = body["id"].as_str() else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "no id" })));
    };
    let accepted = veles_core::devices::answer(
        id,
        veles_core::devices::Answer {
            ok: body["ok"].as_bool().unwrap_or(false),
            result: body["result"].clone(),
        },
    );
    (StatusCode::OK, Json(json!({ "accepted": accepted })))
}

/// POST /api/device/call {device?, action, args} — the `device` tool,
/// from whichever process its session runs in.
async fn device_call(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let Some(action) = body["action"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "no action" })),
        );
    };
    let args = if body["args"].is_object() {
        body["args"].clone()
    } else {
        json!({})
    };
    match veles_core::devices::call(body["device"].as_str(), action, args).await {
        Ok(result) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "result": result })),
        ),
        Err(error) => (StatusCode::OK, Json(json!({ "ok": false, "error": error }))),
    }
}

/// GET /api/device/list — every device that said hello, and what it offers.
async fn device_list() -> Json<Value> {
    Json(veles_core::devices::catalog_json())
}

/// POST /api/desktop/eval/result {id, ok, result} — the desktop's answer.
async fn desktop_eval_result(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(id) = body["id"].as_str() else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "no id" })));
    };
    let waiting = srv.desktop_eval.pending.lock_ok().remove(id);
    match waiting {
        Some(tx) => {
            let _ = tx.send(json!({
                "ok": body["ok"].as_bool().unwrap_or(false),
                "target": "app",
                "result": body["result"].as_str().unwrap_or(""),
            }));
            (StatusCode::OK, Json(json!({ "accepted": true })))
        }
        // Late answers are normal (the request timed out) and are not an
        // error worth alarming the desktop about.
        None => (
            StatusCode::OK,
            Json(json!({ "accepted": false, "note": "nothing was waiting for that id" })),
        ),
    }
}

/// POST /api/desktop/eval/arm {armed} — the desktop reporting its gate.
async fn desktop_eval_arm(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let armed = body["armed"].as_bool().unwrap_or(false);
    srv.desktop_eval
        .armed
        .store(armed, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        "desktop live patching {}",
        if armed { "ARMED" } else { "disarmed" }
    );
    (StatusCode::OK, Json(json!({ "armed": armed })))
}

/// GET /api/desktop/eval/status — is anyone listening, and are they armed?
async fn desktop_eval_status(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    Json(json!({
        "connected": srv.desktop_eval.connected(),
        "armed": srv.desktop_eval.is_armed(),
        "timeout_s": DESKTOP_EVAL_TIMEOUT.as_secs(),
    }))
}

/// POST /api/tcl/reload — re-source every plugin script (manual hot
/// reload; the fs watcher handles the automatic path).
async fn tcl_reload(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    match srv.hooks.reload_plugins().await {
        Ok(()) => Json(json!({"ok": true, "result": "plugins reloaded"})),
        Err(e) => Json(json!({"ok": false, "result": e})),
    }
}

#[derive(Deserialize)]
struct ConfigPathQuery {
    path: String,
}

/// GET /api/config/get?path=a.b.c — one config key, as it stands in the
/// file, plus what changing it would do.
///
/// `set: false` is not the same answer as `value: <default>`: a control
/// that offers on / off / default has to know whether the user ever
/// stated a preference, and the struct defaults cannot tell it.
async fn config_get(Query(q): Query<ConfigPathQuery>) -> (StatusCode, Json<Value>) {
    let path = q.path.trim();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no path supplied" })),
        );
    }
    let doc = match veles_core::configedit::read_doc() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    let segments: Vec<&str> = path.split('.').collect();
    let value = veles_core::configedit::get_json(&doc, &segments);
    let effect = veles_core::configedit::effect_for(path);
    (
        StatusCode::OK,
        Json(json!({
            "path": path,
            "set": value.is_some(),
            "value": value,
            "effect": effect.as_str(),
            "note": effect.note(),
        })),
    )
}

// ── apps (veles_core::apps, PLAN-sessions-and-phone.md A4) ───────────
//
// The window runs an app; these give it the app's files, its own store,
// and its turn with the agent. Every one names the app, and the window
// is the one that checks the person's grants — but the gateway refuses
// what the app never DECLARED, so a window that skipped the check still
// cannot widen an app.

/// GET /api/apps — every app, with its icon, or the reason it is broken.
async fn apps_list() -> Json<Value> {
    Json(veles_core::apps::listing())
}

/// GET /api/apps/:name/source — the app's text files, to run it.
async fn app_source(Path(name): Path<String>) -> (StatusCode, Json<Value>) {
    match (
        veles_core::apps::load(&name),
        veles_core::apps::source(&name),
    ) {
        (Ok(info), Ok(files)) => (StatusCode::OK, Json(json!({"info": info, "files": files}))),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("{e:#}")})),
        ),
    }
}

/// The app has to have declared `cap`, or the gateway refuses.
fn app_declared(
    name: &str,
    cap: &str,
) -> Result<veles_core::apps::AppInfo, (StatusCode, Json<Value>)> {
    let info = veles_core::apps::load(name).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("{e:#}")})),
        )
    })?;
    if !info.capabilities.iter().any(|c| c == cap) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": format!("{name} did not declare {cap:?} in its app.toml")})),
        ));
    }
    Ok(info)
}

/// POST /api/apps/:name/store `{op, key, value}` — the app's own data.
async fn app_store(Path(name): Path<String>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    if let Err(r) = app_declared(&name, "store") {
        return r;
    }
    match veles_core::apps::store(
        &name,
        body["op"].as_str().unwrap_or(""),
        body["key"].as_str().unwrap_or(""),
        body.get("value").cloned(),
    ) {
        Ok(v) => (StatusCode::OK, Json(json!({"value": v}))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("{e:#}")})),
        ),
    }
}

/// Who an app is to the ACL: `app:<name>`, a User unless the owner names
/// it higher — an app asks with what the person granted it, not with the
/// owner's whole hand (USER decision D6).
fn app_principal(name: &str) -> veles_core::acl::Principal {
    let ctx = veles_core::acl::SenderContext {
        platform: Some("app".into()),
        direct: true,
        ..Default::default()
    };
    veles_core::acl::Principal::resolve_ctx_or(
        Some(&format!("app:{name}")),
        &ctx,
        &veles_core::acl::live_config(),
        veles_core::acl::Role::User,
    )
}

/// POST /api/apps/:name/ask `{"text"}` — one turn in the app's own
/// session, as `app:<name>`, answered with the agent's reply.
async fn app_ask(
    State(srv): State<Arc<DesktopServer>>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let info = match app_declared(&name, "ask") {
        Ok(i) => i,
        Err(r) => return r,
    };
    let text = body["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() || text.chars().count() > 4000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "ask needs 1-4000 characters of text"})),
        );
    }
    // The app's session: made once, kept in its store, made again when
    // the person deleted it.
    let existing = veles_core::apps::store(&name, "get", "_session", None)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|sid| srv.store.lock_ok().title(sid).is_ok_and(|t| t.is_some()));
    let sid = match existing {
        Some(s) => s,
        None => {
            let sid = crate::chat::new_session_id();
            if let Err(e) = srv.store.lock_ok().create_session(&sid) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e:#}")})),
                );
            }
            let _ = srv
                .store
                .lock_ok()
                .set_title(&sid, &format!("App: {}", info.title));
            let _ = veles_core::apps::store(&name, "set", "_session", Some(json!(sid)));
            sid
        }
    };
    let agent = match srv.agent_for(&sid) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e:#}")})),
            )
        }
    };
    match run_turn_as(&srv, &agent, &sid, app_principal(&name), "app", &text).await {
        Ok(reply) => (
            StatusCode::OK,
            Json(json!({"reply": reply, "session_id": sid})),
        ),
        Err(e) => (StatusCode::OK, Json(json!({"error": e}))),
    }
}

/// One turn in `sid` as `principal`, from `surface`, then the session's
/// own principal and surface back: an app's ask or a trigger is not who
/// the person is when they open that session and type. Both are set
/// only once the turn lock is held, so no other turn of the session runs
/// under them. Five minutes at most.
async fn run_turn_as(
    srv: &Arc<DesktopServer>,
    agent: &Arc<Agent>,
    sid: &str,
    principal: veles_core::acl::Principal,
    surface: &str,
    text: &str,
) -> std::result::Result<String, String> {
    let turn_lock = srv.turn_lock(sid);
    let _turn = turn_lock.lock().await;
    // The STANDING principal, not `principal()`: that one applies a
    // turn's cap, and restoring a capped view here would make it the
    // session's own.
    let before = agent.standing_principal().map(|p| (*p).clone());
    agent.replace_principal(Some(principal));
    agent.set_turn_surface(Some(surface));
    let mut history = srv.store.lock_ok().load_history(sid).unwrap_or_default();
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        agent.run_turn(&mut history, text, None, |_| {}),
    )
    .await;
    agent.set_turn_surface(None);
    agent.replace_principal(before);
    match r {
        Ok(Ok(r)) => Ok(r.reply),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err("the agent took longer than five minutes".into()),
    }
}

// ── the phone's events (PLAN-sessions-and-phone.md P4) ──────────────

/// A trigger's own session: made at its first fire, remembered in meta,
/// made again when the person deleted it.
fn trigger_session(srv: &DesktopServer, name: &str) -> anyhow::Result<String> {
    let key = veles_core::device_events::session_key(name);
    let store = srv.store.lock_ok();
    if let Some(sid) = store.meta_get(&key) {
        if store.title(&sid).is_ok_and(|t| t.is_some()) {
            return Ok(sid);
        }
    }
    let sid = crate::chat::new_session_id();
    store.create_session(&sid)?;
    store.set_title(&sid, &format!("Trigger: {name}"))?;
    store.meta_set(&key, &sid)?;
    Ok(sid)
}

/// POST /api/device/event {device, kind, source, app, title, text, at} —
/// something happened on a phone (`veles_core::device_events`): the
/// `device_event` hook hears it, and each `platform=device` trigger it
/// matches runs its turn, in that trigger's session, as
/// `trigger:<name>`. Answered at once: the turns run on.
///
/// Passive (`is_passive`): the phone posts every notification while the
/// engine runs, and an event nothing acts on is nobody using it. A turn
/// it starts keeps the engine up by itself.
async fn device_event(
    State(srv): State<Arc<DesktopServer>>,
    Json(ev): Json<veles_core::device_events::DeviceEvent>,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = ev.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    }
    let hook_session = veles_core::device_events::hook_session(&ev);
    let _ = srv
        .hooks
        .fire(
            "device_event",
            &veles_core::hooks::with_session(&hook_session, ev.hook_args()),
        )
        .await;
    let decision = veles_core::device_events::decide(&srv.store, &ev);
    let mut fired = Vec::new();
    for run in decision.runs {
        let name = run.trigger.name.clone();
        let made = trigger_session(&srv, &name).and_then(|sid| Ok((srv.agent_for(&sid)?, sid)));
        let (agent, sid) = match made {
            Ok(x) => x,
            Err(e) => {
                let _ = srv.store.lock_ok().trigger_record_fire(
                    run.trigger.id,
                    &ev.source,
                    "error",
                    Some(&format!("{e:#}")),
                );
                continue;
            }
        };
        let _ = srv.store.lock_ok().trigger_record_fire(
            run.trigger.id,
            &ev.source,
            "fired",
            Some(&format!("a turn in session {sid}")),
        );
        fired.push(json!({ "trigger": name, "session_id": sid }));
        let srv = srv.clone();
        veles_core::rt::spawn(async move {
            let who = veles_core::device_events::trigger_principal(&name);
            if let Err(e) = run_turn_as(&srv, &agent, &sid, who, "trigger", &run.text).await {
                tracing::warn!("trigger {name}: its turn in {sid} failed: {e}");
            }
        });
    }
    let refused: Vec<Value> = decision
        .refused
        .iter()
        .map(|(n, why)| json!({ "trigger": n, "why": why }))
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "fired": fired, "refused": refused })),
    )
}

/// POST /api/pool/scan — what a pool invite would do, before it does
/// it (`veles_core::pool`, PLAN-sessions-and-phone.md P2d). The body is
/// `{"image_b64": …}` (a photo of the QR, JPEG or PNG, as the phone's
/// camera took it) or `{"text": …}` (the invite's text, pasted). Nothing
/// is written: the answer is what the Phone page shows for the owner to
/// confirm, plus the invite's text to send back to `/api/pool/join`.
async fn pool_scan(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let text = if let Some(b64) = body["image_b64"].as_str() {
        use base64::Engine;
        let bytes = match base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("the photo is not base64: {e}")})),
                )
            }
        };
        match veles_core::rt::spawn_blocking(move || veles_core::pool::qr_read(&bytes)).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                let e = format!("{e:#}");
                return (
                    StatusCode::OK,
                    Json(json!({"error": e, "kind": veles_core::pool::scan_error_kind(&e)})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            }
        }
    } else {
        body["text"].as_str().unwrap_or("").to_string()
    };
    match veles_core::pool::decode(&text) {
        Err(e) => {
            let e = format!("{e:#}");
            let kind = veles_core::pool::scan_error_kind(&e);
            (StatusCode::OK, Json(json!({"error": e, "kind": kind})))
        }
        Ok(inv) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (
                StatusCode::OK,
                Json(json!({
                    "text": text,
                    "arbiter": inv.arbiter.name,
                    "arbiter_url": inv.arbiter.url,
                    "name": inv.name,
                    "url": inv.url,
                    "expires_in": inv.expires.saturating_sub(now),
                })),
            )
        }
    }
}

/// POST /api/pool/join `{"text": …}` — join the pool with a scanned
/// invite: the arbiter is called first, and only its yes writes anything
/// (`veles_core::pool::join`). The engine must be restarted afterwards
/// for its A2A server to start; the answer says so.
async fn pool_join(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let inv = match veles_core::pool::decode(body["text"].as_str().unwrap_or("")) {
        Ok(i) => i,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("{e:#}")})),
            )
        }
    };
    match veles_core::pool::join(&inv).await {
        Ok(j) => {
            veles_core::pool::seed_registry(&srv.store.lock_ok(), &j);
            let _ = srv.reload_config();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "arbiter": j.arbiter.name,
                    "members": j.members,
                    "not_introduced": j.not_introduced,
                    "warning": j.written.warning,
                    "restart": true,
                })),
            )
        }
        Err(e) => (StatusCode::OK, Json(json!({"error": format!("{e:#}")}))),
    }
}

/// GET /api/config/form — every option this binary reads, typed, with
/// the value config.toml holds now (`veles_core::settings_form`): what the
/// desktop's All settings page draws its controls from.
async fn config_form() -> Json<Value> {
    Json(veles_core::settings_form::form())
}

/// POST /api/config/set — write one config key.
///
/// Four request shapes, one route, because they are the same edit seen
/// from different controls:
///
///   {"path": "agent.guards.max_iterations", "value": 500}   set
///   {"path": "agent.guards.max_iterations", "value": null}  unset
///   {"path": "plugins.disabled", "add": "noisy"}            append
///   {"path": "plugins.disabled", "remove": "noisy"}         drop
///   {"path": "agent.x", "raw": "5"}                         literal
///
/// `raw` is text a form could not type (an option unset by default and
/// here, so nothing says what it holds): read as a TOML literal by the
/// TOML library, so `5` stays a number, and as a string when it is not
/// one (`settings_form`, the desktop's All settings page).
///
/// Every one goes through `configedit::edit`, so every one keeps the
/// user's comments, backs the file up, and refuses to write a config
/// the agent could not read back.
async fn config_set(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let path = body["path"].as_str().unwrap_or("").trim().to_string();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no path supplied" })),
        );
    }
    let segments: Vec<String> = path.split('.').map(str::to_string).collect();
    if segments.iter().any(|s| s.is_empty()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("`{path}` has an empty path segment") })),
        );
    }
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();

    let add = body.get("add").and_then(|v| v.as_str()).map(str::to_string);
    let remove = body
        .get("remove")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let value = body.get("value").cloned();
    let raw = body.get("raw").and_then(|v| v.as_str()).map(str::to_string);

    let result = veles_core::configedit::edit(&path, |doc| {
        if let Some(text) = raw {
            let tv = veles_core::configedit::literal_or_string(&text);
            return veles_core::configedit::set_value(doc, &refs, tv);
        }
        if let Some(v) = add {
            return veles_core::configedit::array_add(doc, &refs, &v);
        }
        if let Some(v) = remove {
            return veles_core::configedit::array_remove(doc, &refs, &v);
        }
        match value {
            None | Some(Value::Null) => veles_core::configedit::unset(doc, &refs),
            Some(v) => {
                let tv = veles_core::configedit::json_to_toml(&v)?;
                veles_core::configedit::set_value(doc, &refs, tv)
            }
        }
    });
    match result {
        Ok(r) => {
            // The edit landed on disk — pull it into the live handle
            // NOW (R4.5). Writing the file but serving the boot
            // snapshot until a manual /api/config/reload is how a
            // desktop ends up "saying deepseek" after the config says
            // otherwise.
            let _ = srv.reload_config();
            (StatusCode::OK, Json(edit_response(r)))
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/config/reload — re-read config.toml into the live handle.
///
/// Sections that are loaded per use (memory, MCP, ACL, checkpoints,
/// plugins, tui) were already live the moment the file was written;
/// this is for the ones held on the server's own handle. It reports how
/// many cached agents will keep their old snapshot rather than
/// pretending the swap reached them.
async fn config_reload(State(srv): State<Arc<DesktopServer>>) -> (StatusCode, Json<Value>) {
    match srv.reload_config() {
        Ok(stale) => (
            StatusCode::OK,
            Json(json!({
                "reloaded": true,
                "sessions_holding_old_config": stale,
                "note": if stale == 0 {
                    "config reloaded".to_string()
                } else {
                    format!("config reloaded; {stale} open session(s) keep the config they started with")
                },
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/config — what the model picker + settings need.
/// POST /api/config/failover — write the blueprint's failover sections
/// into config.toml, in place.
///
/// USER asked the blueprint editor to be able to "save globally". This
/// is deliberately narrow: it replaces the two failover keys and
/// leaves every other line, and every comment, byte-identical (see
/// configedit::replace_sections). It is not a serializer, because
/// re-emitting a hand-written config would destroy its comments and
/// ordering.
///
/// The previous file is copied to `config.toml.bak` first, and the new
/// content must parse as TOML before anything is written — a config the
/// agent cannot read is worse than an unsaved edit.
async fn save_failover(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let block = body["toml"].as_str().unwrap_or("").trim().to_string();
    if block.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no toml supplied" })),
        );
    }
    let report = veles_core::configedit::edit("failover", |doc| {
        veles_core::configedit::replace_sections(doc, &block, &["fallback_providers", "failover"])
    });
    match report {
        Ok(r) => (StatusCode::OK, Json(edit_response(r))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// The shape every config write answers with. One encoder, so a caller
/// never has to guess whether a given manager reports its effect.
fn edit_response(r: veles_core::configedit::EditReport) -> Value {
    json!({
        "saved": r.path.display().to_string(),
        "backup": r.backup.as_ref().map(|b| b.display().to_string()),
        "effect": r.effect.as_str(),
        "note": r.effect.note(),
    })
}

/// The plugin that carries `script`-mode failover, and the template a
/// fresh one starts from.
///
/// USER: "where/how do you see/edit the tcl script when set to script?"
///
/// Nowhere, was the answer. `script` mode fires the `failover` hook,
/// hooks are provided by `~/.config/veles/plugins/<name>/plugin.tcl`, nothing
/// in the desktop ever showed you one — and on a box where no plugin
/// provides that hook, `script` behaved exactly like `auto` plus a
/// round trip. The blueprint offered the mode without offering the
/// thing the mode runs.
const FAILOVER_PLUGIN: &str = "failover";

const FAILOVER_MANIFEST: &str = r#"name = "failover"
kind = "tcl"
provides_hooks = ["failover"]
entry = "plugin.tcl"
description = "Decides, per failure, whether the chain advances to the next route. Edited from the desktop's failover blueprint."
"#;

const FAILOVER_TEMPLATE: &str = r##"# failover — the hook behind `script` mode in the failover blueprint.
#
# Fires ONCE per failure, before the chain advances to the next route.
#
# In:  ::veles::hook_args is JSON with
#        from_provider  the route that just failed
#        from_model
#        error          the provider's error text
#        session_id
#
# Out: return ""                      -> advance to the next route
#      return {{"action":"deny"}}     -> do not advance; the turn fails
#                                        here with the error above
#
# Anything else is treated as "advance", so a handler that falls off the
# end is safe by default.

namespace eval failover {}

proc failover::handle {} {
    set provider [::veles::json_get $::veles::hook_args from_provider]
    set model    [::veles::json_get $::veles::hook_args from_model]
    set err      [::veles::json_get $::veles::hook_args error]

    # Example: a 401/403 is a credentials problem, not a capacity one.
    # Advancing to the next route will not fix it and burns a second
    # request, so stop and let the error be seen.
    if {[regexp {(?i)\m(401|403|unauthori[sz]ed|invalid api key)\M} $err]} {
        return {{"action":"deny"}}
    }

    # Everything else: let the chain do its job.
    return ""
}

# Re-sourcing this file must not stack a second copy of the handler:
# ::veles::hook appends, and the desktop editor saves by sourcing.
catch { unset ::veles::hooks(failover) }
::veles::hook failover failover::handle
"##;

fn failover_plugin_dir() -> std::path::PathBuf {
    // The loader's root, not a derivation of our own: the hook this
    // editor writes is discovered by `plugins::discover_all` in
    // `plugins::plugins_dir()` (config root). This used to read
    // `veles_home()` (data root) — on a split box the editor saved to
    // one directory and the loader sourced another, so an edit worked
    // in-process (sourced on save) and silently stopped working at the
    // next boot. `plugins_dir()` has no directory parameter, so the
    // wrong root is unrepresentable here.
    veles_core::plugins::plugins_dir().join(FAILOVER_PLUGIN)
}

/// The template a NEW named script starts from. Same `failover::handle`
/// contract as the historic hook, but WITHOUT the trailing
/// `::veles::hook` registration — a named script is sourced and called
/// directly by the engine, and registering it as a hook too would make
/// it fire on the default `script` path as well.
const NAMED_SCRIPT_TEMPLATE: &str = r##"# <name> — a named failover script.
#
# Runs when a [[failover]] rule sets `script = "<name>"`. Fires ONCE per
# failed route, before the chain advances.
#
# In:  ::veles::hook_args is JSON with
#        from_provider  the route that just failed
#        from_model
#        error          the provider's error text
#        session_id
#
# Out: return ""                      -> advance to the next route
#      return {"action":"deny"}       -> do not advance; the turn fails
#                                        here with the error above
#
# Anything else is treated as "advance".

namespace eval failover {}

proc failover::handle {} {
    set provider [::veles::json_get $::veles::hook_args from_provider]
    set model    [::veles::json_get $::veles::hook_args from_model]
    set err      [::veles::json_get $::veles::hook_args error]

    # Example: a 401/403 is a credentials problem, not a capacity one.
    # Advancing to the next route will not fix it and burns a second
    # request, so stop and let the error be seen.
    if {[regexp {(?i)\m(401|403|unauthori[sz]ed|invalid api key)\M} $err]} {
        return {"action":"deny"}
    }

    # Everything else: let the chain do its job.
    return ""
}
"##;

#[derive(Deserialize)]
struct ScriptQuery {
    name: Option<String>,
}

/// GET /api/failover/script[?name=<n>] — the Tcl behind `script` mode.
/// With `?name=` it reads a NAMED script from the failover scripts dir
/// (`failover::scripts_dir()`); without, it reads the historic single
/// hook (a plugin), same as before.
async fn failover_script(
    State(srv): State<Arc<DesktopServer>>,
    Query(q): Query<ScriptQuery>,
) -> Json<Value> {
    if let Some(name) = q.name.as_deref() {
        if !veles_core::failover::is_valid_script_name(name) {
            return Json(json!({
                "name": name,
                "error": "invalid script name (letters, digits, - and _ only, up to 64 chars)",
                "exists": false,
                "source": "",
                "template": "",
            }));
        }
        let path = veles_core::failover::script_path(name);
        let source = std::fs::read_to_string(&path).ok();
        return Json(json!({
            "path": path.display().to_string(),
            "name": name,
            "named": true,
            "exists": source.is_some(),
            "source": source.unwrap_or_else(|| NAMED_SCRIPT_TEMPLATE.to_string()),
            "template": NAMED_SCRIPT_TEMPLATE,
            "hook": "failover::handle",
            "loaded": true,
            "note": format!("sourced and called as failover::handle for any rule with script = \"{name}\""),
        }));
    }

    let dir = failover_plugin_dir();
    let path = dir.join("plugin.tcl");
    let source = std::fs::read_to_string(&path).ok();
    // A plugin in the directory is not necessarily a plugin in the
    // process: an explicit `enabled` list is an allowlist, and the
    // editor has to say so rather than let you edit dead code.
    let loaded = srv.cfg();
    let en = &loaded.plugins.enabled;
    let dis = &loaded.plugins.disabled;
    let loaded = !dis.iter().any(|n| n == FAILOVER_PLUGIN)
        && (en.is_empty() || en.iter().any(|n| n == FAILOVER_PLUGIN));
    Json(json!({
        "path": path.display().to_string(),
        "name": "default",
        "named": false,
        "exists": source.is_some(),
        "source": source.unwrap_or_else(|| FAILOVER_TEMPLATE.to_string()),
        "template": FAILOVER_TEMPLATE,
        "hook": "failover",
        "loaded": loaded,
        "note": if loaded {
            "script mode fires failover::handle for every failed route"
        } else {
            "this plugin is not in [plugins] enabled (or is in disabled), so script mode will fire nothing"
        },
    }))
}

/// GET /api/failover/scripts — the named failover scripts on disk.
async fn list_failover_scripts() -> Json<Value> {
    let dir = veles_core::failover::scripts_dir();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| {
                let e = e.ok()?;
                let stem = e.path().file_stem()?.to_string_lossy().to_string();
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .filter(|x| *x == "tcl")?;
                veles_core::failover::is_valid_script_name(&stem).then_some(stem)
            })
            .collect()
        })
        .unwrap_or_default();
    names.sort();
    Json(json!({ "scripts": names }))
}

/// POST /api/failover/script — write it, but only if it runs.
///
/// The new text is written beside the live file and SOURCED first. A
/// plugin.tcl with an unbalanced brace is not a saved edit, it is a
/// broken hook that fires on the next failure — which is the worst
/// possible moment to find out. Only a script the interpreter accepts
/// is moved into place, and the previous one is kept as plugin.tcl.bak.
async fn save_failover_script(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let source = body["source"].as_str().unwrap_or("");
    if source.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no script supplied" })),
        );
    }
    // A NAMED script (POST body `name`) goes to
    // failover-scripts/<name>.tcl; no name = the historic single hook
    // plugin in plugins/failover/plugin.tcl.
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    let (dir, path, staged, is_named) = if name.is_empty() {
        let dir = failover_plugin_dir();
        let path = dir.join("plugin.tcl");
        let staged = dir.join("plugin.tcl.new");
        (dir, path, staged, false)
    } else {
        if !veles_core::failover::is_valid_script_name(&name) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid script name: {name}") })),
            );
        }
        let dir = veles_core::failover::scripts_dir();
        let path = veles_core::failover::script_path(&name);
        let staged = dir.join(format!("{name}.tcl.new"));
        (dir, path, staged, true)
    };
    let title_note = if is_named {
        format!("named script '{name}'")
    } else {
        "the failover hook plugin".to_string()
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not create {}: {e}", dir.display()) })),
        );
    }
    if let Err(e) = std::fs::write(&staged, source) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not write {}: {e}", staged.display()) })),
        );
    }

    // Source the STAGED copy: this is both the syntax check and the
    // reload, since a successful source has already installed the new
    // procs in the live interpreter.
    //
    // A gateway with no Tcl runtime is a different thing from a broken
    // script. Rejecting the write there would throw away an edit for a
    // reason that has nothing to do with the edit, so it is saved
    // unchecked and the response says exactly that.
    let reloaded = srv.hooks.has_tcl();
    if reloaded {
        let code = format!("source {{{}}}", staged.display());
        if let Err(e) = srv.hooks.eval_tcl(&code).await {
            let _ = std::fs::remove_file(&staged);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("the script did not load: {e}") })),
            );
        }
    }

    if path.exists() {
        let bak = dir.join("plugin.tcl.bak");
        if let Err(e) = std::fs::copy(&path, &bak) {
            let _ = std::fs::remove_file(&staged);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("could not write backup: {e}") })),
            );
        }
    }
    if let Err(e) = std::fs::rename(&staged, &path) {
        let _ = std::fs::remove_file(&staged);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not install the script: {e}") })),
        );
    }
    // A plugin with no manifest is not discovered at the next boot, so
    // the edit would work until the process restarted and then quietly
    // stop working. Write it once, never overwrite a hand-edited one.
    // Named scripts are sourced directly, not discovered as plugins, so
    // they need no manifest.
    if !is_named {
        let manifest = dir.join("plugin.toml");
        if !manifest.exists() {
            let _ = std::fs::write(&manifest, FAILOVER_MANIFEST);
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "saved": path.display().to_string(),
            "name": if is_named { name } else { "default".to_string() },
            "named": is_named,
            "reloaded": reloaded,
            "note": if reloaded {
                format!("{title_note} loaded into the running interpreter — the next failed route uses it")
            } else {
                format!("{title_note} saved, but this process has no Tcl runtime: it was neither checked nor loaded")
            },
        })),
    )
}

async fn config_view(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    // Read what the FILE says, not what the server booted with (R4.5):
    // a best-effort reload first, so a hand edit is visible even if the
    // watcher lost the event. `srv.cfg()` stays the fallback — a broken
    // config.toml must not break the picker that would fix it.
    let _ = srv.reload_config();
    let loaded = srv.cfg();
    let cfg = &loaded;
    let aliases: Vec<Value> = cfg
        .model_aliases
        .iter()
        .map(|(name, a)| {
            json!({
                "name": name,
                "model": a.model,
                "provider": a.provider,
            })
        })
        .collect();
    // Warm discovery before rendering the picker's list. This used to
    // match on ModelsSpec inline, so it could only show hand-enumerated
    // models — useless against a router serving hundreds. Best-effort:
    // a provider whose catalog will not answer keeps exactly its
    // curated list, and `discovered` says which is which so the overlay
    // can tell a live list from a configured one.
    veles_core::discovery::warm_all(cfg).await;
    let providers: Vec<Value> = cfg
        .custom_providers
        .iter()
        .map(|p| {
            let merged = veles_core::discovery::merged_cached(p);
            let models: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
            json!({
                "name": p.name.clone(),
                "models": models,
                "discovered": merged.iter().any(|m| m.discovered),
            })
        })
        .collect();
    let default_model = cfg.model.default.clone().unwrap_or_default();
    let default_provider = cfg.model.provider.clone().unwrap_or_default();
    let reasoning = cfg.agent.reasoning_effort.clone().unwrap_or_default();
    Json(json!({
        "default_model": default_model,
        "default_provider": default_provider,
        "reasoning_effort": reasoning,
        "aliases": aliases,
        "providers": providers,
        "fallback_providers": cfg.fallback_providers.iter().map(|f| json!({"provider": f.provider, "model": f.model})).collect::<Vec<_>>(),
        // The failover POLICY, not just the chain: the blueprint editor
        // needs mode/hops/aux per scope to draw what actually happens
        // when a route fails.
        "failover": cfg.failover_rules.iter().map(|r| json!({
            "provider": r.provider,
            "model": r.model,
            "mode": r.mode(),
            "max_hops": r.max_hops,
            "wait_default_secs": r.wait_default_secs,
            "aux_fallback_provider": r.aux_fallback_provider,
            "aux_fallback_model": r.aux_fallback_model,
        })).collect::<Vec<_>>(),
        "memory_limits": {
            "memory": cfg.memory.memory_char_limit,
            "user": cfg.memory.user_char_limit,
        },
    }))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_search_limit")]
    limit: u32,
}
fn default_search_limit() -> u32 {
    25
}

/// GET /api/search?q=... — FTS over the transcript (hermes find + session
/// search parity). Returns snippets grouped by session.
async fn search(
    State(srv): State<Arc<DesktopServer>>,
    Query(p): Query<SearchQuery>,
) -> Json<Value> {
    // A full-text search over every message: off the async workers, on a
    // pooled connection.
    let hits = {
        let (srv, q, limit) = (srv.clone(), p.q.clone(), p.limit.clamp(1, 200));
        veles_core::rt::spawn_blocking(move || {
            srv.read("desktop::search", |s| s.search_full(&q, limit))
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
    };
    let out: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            json!({
                "session_id": h.session_id,
                "msg_id": h.msg_id,
                "role": h.role,
                "snippet": h.snippet,
                "created_at": h.created_at,
            })
        })
        .collect();
    Json(json!({ "hits": out, "query": p.q }))
}

/// GET /api/skills[?session=<id>] — every skill on disk and whether it
/// reaches the prompt.
///
/// This listed names and descriptions and nothing else, which is why
/// the Skills overlay could only ever be a viewer. It now reports, per
/// skill: whether it is switched on, whether the session overrode the
/// global default, and whether veles is allowed to edit it at all
/// (hermes-imported skills are someone else's library).
///
/// Every skill is listed regardless of state — a switched-off skill
/// still needs a row to switch back on.
async fn skills_list(
    State(srv): State<Arc<DesktopServer>>,
    Query(p): Query<StdMap<String, String>>,
) -> Json<Value> {
    let session = p.get("session").cloned().unwrap_or_default();
    let sel = if session.is_empty() {
        veles_core::skills::SkillSelection::global()
    } else {
        veles_core::skills::selection_for(&srv.store, &session)
    };
    let set = veles_core::skills::SkillSet::load();
    let out: Vec<Value> = set
        .skills
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "category": s.category,
                "root": s.root,
                "enabled": sel.allows(&s.name),
                // Three states, not two: "on for this session", "off for
                // this session", and "whatever config says".
                "session_override": sel.session.get(&s.name),
                "disabled_by_default": sel.disabled.iter().any(|d| d == &s.name),
                "editable": s.root == "veles",
                "path": s.dir.join("SKILL.md").display().to_string(),
            })
        })
        .collect();
    let on = out.iter().filter(|s| s["enabled"] == json!(true)).count();
    Json(json!({
        "skills": out,
        "count": out.len(),
        "enabled_count": on,
        "session": session,
    }))
}

/// POST /api/sessions/:id/skill {name, value: on|off|default} — switch a
/// skill for one session, and rebuild that session's prompt so it takes
/// effect on the very next turn rather than the next session.
async fn session_skill(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let agent = match srv.agent_for(&id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": e.to_string() })),
            )
        }
    };
    let a = &agent;
    let name = body["name"].as_str().unwrap_or("");
    if name.is_empty() {
        // Read, same convention as /mechanism: no name means "tell me".
        let overrides: Vec<Value> = a
            .skill_overrides()
            .into_iter()
            .map(|(k, v)| json!({ "name": k, "enabled": v }))
            .collect();
        return (StatusCode::OK, Json(json!({ "overrides": overrides })));
    }
    match a.set_skill(name, body["value"].as_str()) {
        Ok(msg) => (
            StatusCode::OK,
            Json(json!({ "message": msg, "name": name })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// POST /api/skills/new {name, description?, category?, content?}
async fn skill_new(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let name = body["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a skill needs a name" })),
        );
    }
    let description = body["description"].as_str().unwrap_or("");
    // A blank new skill would be discovered and then ignored (no
    // description = nothing in the index), so seed a valid one.
    let content = body["content"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| veles_core::skills::skill_template(name, description));
    match veles_core::skills::create_skill(name, body["category"].as_str().unwrap_or(""), &content)
    {
        Ok(dir) => (
            StatusCode::CREATED,
            Json(json!({
                "name": name,
                "dir": dir.display().to_string(),
                "path": dir.join("SKILL.md").display().to_string(),
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/skills/install {url, name?, overwrite?} — the git install
/// that only `veles skills install` could reach.
async fn skill_install(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let url = body["url"].as_str().unwrap_or("").trim();
    if url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no url supplied" })),
        );
    }
    let report = veles_core::skills::install(
        url,
        body["name"].as_str().filter(|s| !s.trim().is_empty()),
        body["overwrite"].as_bool().unwrap_or(false),
    );
    // `install` reports in prose and signals failure with an "error:"
    // prefix; keep the prose (it is genuinely the useful part) and map
    // the status so the UI does not have to parse it.
    let failed = report.starts_with("error:") || report.starts_with("multiple skills");
    (
        if failed {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::CREATED
        },
        Json(if failed {
            json!({ "error": report })
        } else {
            json!({ "message": report })
        }),
    )
}

/// POST /api/skills/:name — replace a skill's SKILL.md.
async fn skill_save(
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(content) = body["content"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no content supplied" })),
        );
    };
    match veles_core::skills::write_skill(&name, content) {
        Ok(path) => (
            StatusCode::OK,
            Json(json!({ "saved": path.display().to_string() })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// DELETE /api/skills/:name
async fn skill_delete(Path(name): Path<String>) -> (StatusCode, Json<Value>) {
    match veles_core::skills::delete_skill(&name) {
        Ok(dir) => (
            StatusCode::OK,
            Json(json!({ "removed": dir.display().to_string() })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/skills/:name?file=... — render a skill (SKILL.md or linked file).
async fn skill_view(
    State(_srv): State<Arc<DesktopServer>>,
    Path(name): Path<String>,
    Query(p): Query<StdMap<String, String>>,
) -> Json<Value> {
    let set = veles_core::skills::SkillSet::load();
    let file = p.get("file").map(|s| s.as_str());
    let body = veles_core::skills::view_skill(&set, &name, file, None, None);
    Json(json!({ "name": name, "body": body }))
}

/// Resolve the `memory` / `user` selector both memory routes take.
fn memory_kind(name: &str) -> Option<veles_core::memory::StoreKind> {
    match name {
        "memory" | "core" => Some(veles_core::memory::StoreKind::Memory),
        "user" | "profile" => Some(veles_core::memory::StoreKind::User),
        _ => None,
    }
}

/// POST /api/memory {store, entries|ops} — edit a memory store.
///
/// Two shapes, because the UI and the agent want different things:
///
///   {"store":"memory","entries":["…","…"]}   replace the whole list
///   {"store":"memory","ops":[{...}]}         add/replace/remove ops
///
/// The overlay uses `entries` (it is editing a list in front of the
/// user); `ops` is the same surface the `memory` tool drives, budget
/// checks included. Both go through `MemoryStore`, so every write is
/// a revision in the memory history and is rollbackable.
async fn memory_write(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let Some(kind) = memory_kind(body["store"].as_str().unwrap_or("memory")) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "store must be `memory` or `user`" })),
        );
    };
    let ms = veles_core::memory::MemoryStore::new();

    if let Some(ops) = body["ops"].as_array() {
        let report = ms.apply(kind, ops);
        return (StatusCode::OK, Json(json!({ "message": report })));
    }
    let Some(entries) = body["entries"].as_array() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "send `entries` (a list) or `ops`" })),
        );
    };
    let entries: Vec<String> = entries
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // The budget is the reason the agent's `memory` tool refuses writes,
    // and a UI that silently blew past it would put the session over the
    // limit at the next prompt build instead of here, where it can be
    // fixed.
    let limits = ms.limits();
    let limit = match kind {
        veles_core::memory::StoreKind::Memory => limits.memory_char_limit,
        veles_core::memory::StoreKind::User => limits.user_char_limit,
    };
    let limit = limit as usize;
    let size: usize = entries.iter().map(|e| e.chars().count()).sum::<usize>()
        + entries.len().saturating_sub(1) * veles_core::memory::DELIM.chars().count();
    if size > limit {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("{size} characters exceeds the {limit}-character budget for this store"),
            })),
        );
    }
    match ms.write(kind, &entries) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "written": entries.len(), "chars": size, "limit": limit })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/memory/log?limit=&path= — the revision history, newest
/// first: `log` (one line each, what the History dialog shows) and
/// `rows` (the same, structured), for every file or just `path`.
async fn memory_log(Query(p): Query<StdMap<String, String>>) -> (StatusCode, Json<Value>) {
    let n = p
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(1, 200);
    let path = p.get("path").map(String::as_str).filter(|s| !s.is_empty());
    match veles_core::memory::MemoryStore::new().log_rows(path, n) {
        Ok(rows) => {
            let lines: Vec<String> = rows.iter().map(veles_core::memory::log_line).collect();
            (StatusCode::OK, Json(json!({ "log": lines, "rows": rows })))
        }
        Err(e) => memory_error(e),
    }
}

/// A store error as the desktop should see it: a [`Conflict`] is 409
/// (with the sha to reload against), a missing file 404, the rest 400.
///
/// [`Conflict`]: veles_core::memory::Conflict
fn memory_error(e: anyhow::Error) -> (StatusCode, Json<Value>) {
    if let Some(c) = e.downcast_ref::<veles_core::memory::Conflict>() {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": c.to_string(), "current_sha": c.current_sha })),
        );
    }
    let msg = format!("{e:#}");
    let code = if msg.starts_with("no such memory file") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    (code, Json(json!({ "error": msg })))
}

/// GET /api/memory/files — every memory file: path, chars, rev, at,
/// source, sha256. Memory is in the database; this is its directory.
async fn memory_files() -> (StatusCode, Json<Value>) {
    match veles_core::memory::MemoryStore::new().list() {
        Ok(files) => (StatusCode::OK, Json(json!({ "files": files }))),
        Err(e) => memory_error(e),
    }
}

/// GET /api/memory/file?path=&rev= — one file's content (the newest,
/// or revision `rev`), with the sha a save must name as its base.
async fn memory_file_get(Query(p): Query<StdMap<String, String>>) -> (StatusCode, Json<Value>) {
    let ms = veles_core::memory::MemoryStore::new();
    let path = p.get("path").cloned().unwrap_or_default();
    if let Some(rev) = p.get("rev").filter(|r| !r.is_empty()) {
        let Ok(rev) = rev.trim_start_matches('r').parse::<i64>() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("not a revision: {rev}") })),
            );
        };
        return match ms.content_at(&path, rev) {
            Ok(content) => (
                StatusCode::OK,
                Json(json!({ "path": path, "rev": rev, "content": content })),
            ),
            Err(e) => memory_error(e),
        };
    }
    match ms.file(&path) {
        Ok(Some((f, content))) => (
            StatusCode::OK,
            Json(json!({ "path": f.path, "rev": f.rev, "sha256": f.sha256, "content": content })),
        ),
        Ok(None) => memory_error(anyhow::anyhow!("no such memory file: {path}")),
        Err(e) => memory_error(e),
    }
}

/// POST /api/memory/file {path, content, base_sha} — save a file.
///
/// `base_sha` is REQUIRED: the sha the editor opened (from GET
/// /api/memory/file), or null for a new file. If the file moved on
/// since — the agent wrote it — nothing is overwritten and the answer
/// is 409, so the panel can say "changed since you opened it".
async fn memory_file_put(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let path = body["path"].as_str().unwrap_or_default();
    let Some(content) = body["content"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "send `content`" })),
        );
    };
    let Some(base) = body.get("base_sha") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": "send `base_sha` — the sha you opened, or null for a new file" }),
            ),
        );
    };
    let base = base.as_str();
    match veles_core::memory::MemoryStore::new().replace_if(path, content, base, "desktop") {
        Ok(recorded) => (
            StatusCode::OK,
            Json(json!({
                "path": path,
                "recorded": recorded,
                "sha256": veles_core::memory::MemoryStore::content_sha(content),
            })),
        ),
        Err(e) => memory_error(e),
    }
}

/// POST /api/memory/file/delete {path} — delete (a revision; restorable).
async fn memory_file_delete(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let path = body["path"].as_str().unwrap_or_default();
    match veles_core::memory::MemoryStore::new().delete_file(path) {
        Ok(()) => (StatusCode::OK, Json(json!({ "deleted": path }))),
        Err(e) => memory_error(e),
    }
}

/// POST /api/memory/file/move {from, to} — rename (one transaction).
async fn memory_file_move(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let from = body["from"].as_str().unwrap_or_default();
    let to = body["to"].as_str().unwrap_or_default();
    match veles_core::memory::MemoryStore::new().move_file(from, to) {
        Ok(()) => (StatusCode::OK, Json(json!({ "from": from, "to": to }))),
        Err(e) => memory_error(e),
    }
}

/// POST /api/memory/restore {path, rev} — make an old revision current.
async fn memory_restore(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let path = body["path"].as_str().unwrap_or_default();
    let Some(rev) = body["rev"].as_i64() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "send `rev` (a revision id)" })),
        );
    };
    match veles_core::memory::MemoryStore::new().restore(path, rev) {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "path": path, "restored": rev })),
        ),
        Err(e) => memory_error(e),
    }
}

/// GET /api/memory/diff?path=&old=&new= — unified diff between two
/// revisions (default: the file's last change).
async fn memory_diff(Query(p): Query<StdMap<String, String>>) -> (StatusCode, Json<Value>) {
    let rev = |k: &str| -> Result<Option<i64>, String> {
        match p.get(k).map(String::as_str).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(r) => r
                .trim_start_matches('r')
                .parse::<i64>()
                .map(Some)
                .map_err(|_| format!("not a revision: {r}")),
        }
    };
    let (old, new) = match (rev("old"), rev("new")) {
        (Ok(o), Ok(n)) => (o, n),
        (Err(e), _) | (_, Err(e)) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
        }
    };
    let path = p.get("path").cloned().unwrap_or_default();
    match veles_core::memory::MemoryStore::new().diff(&path, old, new) {
        Ok(d) => (StatusCode::OK, Json(json!({ "path": path, "diff": d }))),
        Err(e) => memory_error(e),
    }
}

/// POST /api/memory/rollback {store|path, n} — undo N revisions.
async fn memory_rollback(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let path = match body["path"].as_str().filter(|s| !s.trim().is_empty()) {
        Some(p) => p.to_string(),
        None => match memory_kind(body["store"].as_str().unwrap_or("memory")) {
            Some(k) => k.hier_path().to_string(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "send a `path`, or a `store` of `memory`/`user`" })),
                )
            }
        },
    };
    let n = body["n"].as_u64().unwrap_or(1).clamp(1, 100) as usize;
    match veles_core::memory::MemoryStore::new().rollback(&path, n) {
        Ok(msg) => (
            StatusCode::OK,
            Json(json!({ "message": msg, "path": path })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/memory — entries of both stores (parity: memory panel).
async fn memory_view(State(_srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let ms = veles_core::memory::MemoryStore::new();
    let limits = ms.limits();
    Json(json!({
        "memory": ms.entries(veles_core::memory::StoreKind::Memory),
        "user": ms.entries(veles_core::memory::StoreKind::User),
        "limits": { "memory": limits.memory_char_limit, "user": limits.user_char_limit },
        "usage": {
            "memory": ms.read(veles_core::memory::StoreKind::Memory).chars().count(),
            "user": ms.read(veles_core::memory::StoreKind::User).chars().count(),
        },
    }))
}

/// GET /api/plugins — discovered plugins + enabled state.
/// GET /api/hooks — every registered hook handler and whether it runs.
///
/// Hooks had NO surface at all: they exist only as an array inside the
/// gateway's Tcl interpreter, so nothing outside it could name one. The
/// interpreter answers `::veles::hooks_json`; this is that answer, plus
/// the ownership map (which plugin declares each hook, from the
/// manifests) so a row can say where a handler came from and open it.
///
/// `has_tcl` matters: "this gateway was built without a plugin runtime"
/// and "no hooks are registered" look identical in an empty list, and
/// they are not the same problem.
async fn hooks_list(State(srv): State<Arc<DesktopServer>>) -> (StatusCode, Json<Value>) {
    if !srv.hooks.has_tcl() {
        return (
            StatusCode::OK,
            Json(json!({
                "has_tcl": false,
                "hooks": [],
                "note": "this gateway has no Tcl interpreter — no plugin can register a hook",
            })),
        );
    }
    let raw = match srv.hooks.eval_tcl("::veles::hooks_json").await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("hook introspection failed: {e}") })),
            )
        }
    };
    let mut hooks: Value = serde_json::from_str(raw.trim()).unwrap_or_else(
        |e| json!([{ "name": "?", "handlers": [], "error": format!("unparsable: {e}") }]),
    );

    // Which plugin PROVIDES each hook, from the manifests — so the UI
    // can offer "open the plugin that registered this".
    let mut providers: HashMap<String, Vec<Value>> = HashMap::new();
    for p in veles_core::plugins::discover_all().unwrap_or_default() {
        for h in &p.manifest.provides_hooks {
            providers.entry(h.clone()).or_default().push(json!({
                "plugin": p.manifest.name,
                "path": p.dir.join(&p.manifest.entry).display().to_string(),
            }));
        }
    }
    if let Some(list) = hooks.as_array_mut() {
        for hook in list.iter_mut() {
            let name = hook["name"].as_str().unwrap_or("").to_string();
            hook["provided_by"] = json!(providers.get(&name).cloned().unwrap_or_default());
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "has_tcl": true,
            "hooks": hooks,
            // What the engine can fire, so the manager can also show the
            // hooks nothing has registered for yet.
            "known": veles_core::hooks::KNOWN_HOOKS,
        })),
    )
}

/// POST /api/hooks/toggle {hook, handler, enabled} — switch one handler.
///
/// Two writes, deliberately: the interpreter so it takes effect on the
/// very next fire, and `[hooks] disabled` in config.toml so it survives
/// a reload (which re-sources every plugin and re-registers its
/// handlers) and a restart.
async fn hooks_toggle(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let hook = body["hook"].as_str().unwrap_or("").trim().to_string();
    let handler = body["handler"].as_str().unwrap_or("").trim().to_string();
    let enabled = body["enabled"].as_bool().unwrap_or(true);
    if hook.is_empty() || handler.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "both `hook` and `handler` are required" })),
        );
    }
    // A space would break the config entry's one-separator rule, and
    // both a hook name and a proc name are single words.
    if hook.contains(char::is_whitespace) || handler.contains(char::is_whitespace) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "hook and handler names cannot contain whitespace" })),
        );
    }
    if !srv.hooks.has_tcl() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "this gateway has no Tcl interpreter" })),
        );
    }
    let verb = if enabled {
        "hook_enable"
    } else {
        "hook_disable"
    };
    let script = format!("::veles::{verb} {{{hook}}} {{{handler}}}");
    if let Err(e) = srv.hooks.eval_tcl(&script).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not {verb}: {e}") })),
        );
    }
    let entry = veles_core::config::HooksSection::entry(&hook, &handler);
    let edited = veles_core::configedit::edit("hooks", |doc| {
        if enabled {
            veles_core::configedit::array_remove(doc, &["hooks", "disabled"], &entry)
        } else {
            veles_core::configedit::array_add(doc, &["hooks", "disabled"], &entry)
        }
    });
    match edited {
        Ok(r) => {
            let mut resp = edit_response(r);
            resp["hook"] = json!(hook);
            resp["handler"] = json!(handler);
            resp["enabled"] = json!(enabled);
            // The interpreter already has it; the config write is only
            // about surviving a reload.
            resp["note"] = json!("in effect now, and kept across reloads");
            (StatusCode::OK, Json(resp))
        }
        Err(e) => (
            StatusCode::OK,
            Json(json!({
                "hook": hook,
                "handler": handler,
                "enabled": enabled,
                "note": "in effect now, but NOT saved — it will come back on the next plugin reload",
                "error": format!("{e:#}"),
            })),
        ),
    }
}

/// GET /api/plugins — every plugin on disk and whether it is loaded.
///
/// This used to list `discover(...)`, which FILTERS the disabled ones
/// out — so the `enabled` field it computed could never be false, a
/// switched-off plugin disappeared from the UI, and there was no row
/// left to switch it back on with. It lists `discover_all` now and
/// reports state per row.
///
/// `enabled` is read from config on every call rather than from the
/// server's snapshot, because the plugin loader does the same
/// (`tclrt::load_plugins`) — a toggle is live for both or neither.
async fn plugins_list(State(_srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let loaded = veles_core::config::load().unwrap_or_default();
    let enabled = loaded.plugins.enabled.clone();
    let disabled = loaded.plugins.disabled.clone();
    let found = veles_core::plugins::discover_all().unwrap_or_default();
    let out: Vec<Value> = found
        .iter()
        .map(|p| {
            let name = &p.manifest.name;
            json!({
                "name": name,
                "kind": p.manifest.kind,
                "description": p.manifest.description,
                "hooks": p.manifest.provides_hooks,
                "entry": p.manifest.entry,
                "dir": p.dir.display().to_string(),
                "path": p.dir.join(&p.manifest.entry).display().to_string(),
                // From the read-only bundled tier rather than the user's
                // plugins dir: not removable, and an edit belongs in a
                // user copy of the same name, which then wins.
                "bundled": veles_core::plugins::is_bundled(p),
                "enabled": veles_core::plugins::is_enabled(name, &enabled, &disabled),
                // WHY it is off, so the UI can say "you disabled this"
                // rather than "not in the allowlist" when both are true.
                "explicitly_disabled": disabled.iter().any(|d| d == name),
                "allowlisted": enabled.is_empty() || enabled.iter().any(|e| e == name),
            })
        })
        .collect();
    Json(json!({
        "plugins": out,
        // An allowlist changes what "not mentioned" means, and the UI
        // has to say so or a newly installed plugin looks broken.
        "allowlist_active": !enabled.is_empty(),
    }))
}

/// GET /api/process-plugins — the gateway's process plugins, one row
/// per instance (state, pid, version, restarts, last error, lease), and
/// the ones refused at start. `veles plugins status` prints this.
async fn process_plugins_status() -> Json<Value> {
    process_plugins_status_with(veles_core::pluginhost::global())
}

fn process_plugins_status_with(host: Option<&veles_core::pluginhost::PluginHost>) -> Json<Value> {
    match host {
        None => Json(json!({ "host": false, "instances": [] })),
        Some(h) => Json(json!({ "host": true, "instances": h.status() })),
    }
}

/// POST /api/process-plugins/:target/reload|stop|start — `target` is
/// `name` or `name:instance`. Waits for each instance's answer (a reload
/// is a whole shutdown and handshake), unlike the chat command, which
/// files the request and returns.
async fn process_plugin_control(
    Path((target, action)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    process_plugin_control_with(veles_core::pluginhost::global(), &target, &action).await
}

async fn process_plugin_control_with(
    host: Option<&veles_core::pluginhost::PluginHost>,
    target: &str,
    action: &str,
) -> (StatusCode, Json<Value>) {
    if !matches!(action, "reload" | "stop" | "start") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown action `{action}`: reload | stop | start") })),
        );
    }
    let Some(h) = host else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "error": "this process runs no plugin host (process plugins run in the gateway)" }),
            ),
        );
    };
    let r = match action {
        "reload" => h.reload(target).await,
        "stop" => h.stop(target).await,
        _ => h.start_again(target).await,
    };
    match r {
        Ok(lines) => (StatusCode::OK, Json(json!({ "results": lines }))),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))),
    }
}

/// GET /api/process-plugins/:target/logs — the last lines each instance
/// of `target` wrote to its stderr (the whole history is in the
/// gateway's log, target `veles_core::plugin`).
async fn process_plugin_logs(Path(target): Path<String>) -> (StatusCode, Json<Value>) {
    let Some(h) = veles_core::pluginhost::global() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "this process runs no plugin host" })),
        );
    };
    match h.logs(&target) {
        Ok(v) => (
            StatusCode::OK,
            Json(json!({
                "logs": v
                    .into_iter()
                    .map(|(instance, lines)| json!({ "instance": instance, "lines": lines }))
                    .collect::<Vec<_>>()
            })),
        ),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))),
    }
}

/// GET /api/plugins/services — the service registry's dependency view
/// (`veles_core::services::report`): every service provided, declared or
/// used here, who answers it, who uses it, and whether a required use
/// has no provider.
async fn plugins_services() -> Json<Value> {
    Json(veles_core::services::report_json())
}

/// GET /api/plugins/overrides — the override points (PLAN-extension-api.md
/// D2): every decision core asks by name, its summary, and the providers
/// that answer it, top (answering) first, the default last.
async fn plugins_overrides() -> Json<Value> {
    Json(veles_core::services::overrides_json())
}

/// GET /api/plugins/runtime — BOTH plugin tiers, one row per instance,
/// tagged by `tier` ("process" | "module"). The generic name for the
/// surface M6 calls `/api/plugins*`; `/api/process-plugins` stays as the
/// process-only view.
async fn plugins_runtime_status() -> Json<Value> {
    let process: Vec<Value> = veles_core::pluginhost::global()
        .map(|h| {
            h.status()
                .into_iter()
                .map(|s| {
                    let mut v = serde_json::to_value(&s).unwrap_or(json!({}));
                    v["tier"] = json!("process");
                    v
                })
                .collect()
        })
        .unwrap_or_default();
    let modules: Vec<Value> = veles_core::modulehost::global()
        .map(|h| {
            h.status()
                .into_iter()
                .map(|s| {
                    let mut v = serde_json::to_value(&s).unwrap_or(json!({}));
                    v["tier"] = json!("module");
                    v
                })
                .collect()
        })
        .unwrap_or_default();
    // The Tcl tier (R7): one row per enabled Tcl plugin, `loaded` or
    // `unloaded` — the only states a script has.
    let tcl: Vec<Value> = veles_core::tclrt::plugin_rows()
        .into_iter()
        .map(|(name, state)| {
            json!({
                "tier": "tcl",
                "plugin": name,
                "instance": "",
                "state": state,
                "version": "",
                "restarts": 0,
            })
        })
        .collect();
    // The native tier (R8): one row per enabled `.so` plugin, `loaded`,
    // `unloaded` or `refused` (with the reason as its last error).
    let so: Vec<Value> = veles_core::rustplugins::so_rows()
        .into_iter()
        .map(|(name, state, why)| {
            json!({
                "tier": "so",
                "plugin": name,
                "instance": "",
                "state": state,
                "version": "",
                "restarts": 0,
                "last_error": why,
            })
        })
        .collect();
    // The script tier (R9): one row per enabled plugin of a script kind
    // (`js`, `racket`, `lisp`): loaded | error | unloaded | pending |
    // no-host, with its kind.
    let script: Vec<Value> = veles_core::scripthost::rows()
        .into_iter()
        .map(|row| {
            json!({
                "tier": "script",
                "kind": row.kind,
                "plugin": row.name,
                "instance": "",
                "state": row.state,
                "version": "",
                "restarts": 0,
                "last_error": row.detail,
            })
        })
        .collect();
    let host =
        veles_core::pluginhost::global().is_some() || veles_core::modulehost::global().is_some();
    Json(json!({
        "host": host,
        "instances": process.into_iter().chain(modules).chain(so).chain(tcl).chain(script).collect::<Vec<_>>(),
    }))
}

/// POST /api/plugins/runtime/rescan — re-discover under both hosts'
/// roots and start anything new. A first install needs no gateway
/// restart.
async fn plugins_runtime_rescan() -> Json<Value> {
    let mut started: Vec<String> = Vec::new();
    if let Some(m) = veles_core::modulehost::global() {
        started.extend(m.rescan().await.into_iter().map(|l| format!("module:{l}")));
    }
    // The process tier has no rescan of its own yet (R3 names it; the
    // module host's rescan is the one this phase builds).
    Json(json!({ "started": started }))
}

/// POST /api/plugins/runtime/:target/reload|stop|start|unload — act on
/// whichever tier hosts `target`: a module, a process plugin, (R8) a
/// native `.so` plugin, or (R7) a Tcl plugin by name. A target known to
/// none 404s.
async fn plugins_runtime_control(
    Path((target, action)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    if !matches!(action.as_str(), "reload" | "stop" | "start" | "unload") {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": format!("unknown action `{action}`: reload | stop | start | unload") }),
            ),
        );
    }
    // Modules first: a module and a process plugin of the same name is
    // a misconfiguration, but the module tier is the one being built.
    if let Some(m) = veles_core::modulehost::global() {
        if let Ok(r) = match action.as_str() {
            "reload" => m.reload(&target).await,
            "stop" => m.stop(&target).await,
            "unload" => m.unload(&target).await,
            _ => m.start_again(&target).await,
        } {
            return (
                StatusCode::OK,
                Json(json!({ "tier": "module", "results": r })),
            );
        }
    }
    // The native tier (R8), by name: `unload`/`stop` purge everything
    // the `.so` owns and unmap it when nothing still runs its code;
    // `start`/`reload` dlopen it again (a loaded one is reloaded).
    if veles_core::rustplugins::so_rows()
        .iter()
        .any(|(name, _, _)| *name == target)
    {
        let name = target.clone();
        let unload = matches!(action.as_str(), "unload" | "stop");
        let r = veles_core::rt::spawn_blocking(move || {
            if unload {
                veles_core::rustplugins::unload_so(&name)
            } else {
                veles_core::rustplugins::start_so(&name)
            }
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
        return match r {
            Ok(line) => (
                StatusCode::OK,
                Json(json!({ "tier": "so", "results": [line] })),
            ),
            Err(e) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{e:#}") })),
            ),
        };
    }
    // The script tier (R9), by name: its host takes it out or loads it
    // through the seam, and the answer is the seam's line.
    if let Some(fut) = veles_core::scripthost::request(&target, &action) {
        return match fut.await {
            Ok(line) => (
                StatusCode::OK,
                Json(json!({ "tier": "script", "results": [line] })),
            ),
            Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
        };
    }
    // The Tcl tier, by name: `unload`/`stop` drop its handlers and
    // registrations, `start`/`reload` source it again.
    if veles_core::tclrt::plugin_rows()
        .iter()
        .any(|(name, _)| *name == target)
    {
        let Some(handle) = veles_core::tclrt::global_handle() else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "this process runs no plugins interpreter" })),
            );
        };
        let r = if matches!(action.as_str(), "unload" | "stop") {
            handle.unload_plugin(&target).await
        } else {
            handle.load_plugin(&target).await
        };
        return match r {
            Ok(line) => (
                StatusCode::OK,
                Json(json!({ "tier": "tcl", "results": [line] })),
            ),
            Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
        };
    }
    if let Some(h) = veles_core::pluginhost::global() {
        let r = match action.as_str() {
            "reload" => h.reload(&target).await,
            "stop" => h.stop(&target).await,
            "unload" => h.unload(&target).await,
            _ => h.start_again(&target).await,
        };
        return match r {
            Ok(lines) => (
                StatusCode::OK,
                Json(json!({ "tier": "process", "results": lines })),
            ),
            Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))),
        };
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "this process runs no plugin host" })),
    )
}

/// POST /api/plugins/:name/enable|disable — flip a plugin, live.
///
/// `disabled` is the switch; `enabled` is an allowlist that is only
/// consulted when non-empty. So enabling means "not in disabled, and
/// in enabled IF an allowlist is in force" — leaving the allowlist
/// alone when there isn't one, because adding the first entry would
/// silently switch every other plugin off.
///
/// The write lands in config.toml and is followed by a plugin reload,
/// and `load_plugins` re-reads config itself — so this takes effect
/// without a restart.
async fn plugin_set_enabled(
    State(srv): State<Arc<DesktopServer>>,
    Path((name, action)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    // Anything but the switch is a MODULE's route under its name (R5:
    // `/api/plugins/slopdrop/sync`), with the JSON body handed over.
    if action != "enable" && action != "disable" {
        return module_route_post(&name, &action, body.map(|Json(v)| v)).await;
    }
    let name = match veles_core::plugins::validate_name(&name) {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
        }
    };
    let on = match action.as_str() {
        "enable" => true,
        "disable" => false,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown action `{other}`") })),
            )
        }
    };
    let allowlist_active = !veles_core::config::load()
        .unwrap_or_default()
        .plugins
        .enabled
        .is_empty();

    let edited = veles_core::configedit::edit("plugins", |doc| {
        if on {
            veles_core::configedit::array_remove(doc, &["plugins", "disabled"], &name)?;
            if allowlist_active {
                veles_core::configedit::array_add(doc, &["plugins", "enabled"], &name)?;
            }
            Ok(())
        } else {
            veles_core::configedit::array_add(doc, &["plugins", "disabled"], &name)?;
            // Leaving a stale allowlist entry would make the row read
            // "allowlisted" while it is switched off — true, and useless.
            veles_core::configedit::array_remove(doc, &["plugins", "enabled"], &name)
        }
    });
    let report = match edited {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    let mut resp = edit_response(report);
    resp["name"] = json!(name);
    resp["enabled"] = json!(on);
    // Re-source so the change is visible now. A reload failure is worth
    // reporting but does not un-write the config.
    match srv.hooks.reload_plugins().await {
        Ok(()) => resp["reloaded"] = json!(true),
        Err(e) => {
            resp["reloaded"] = json!(false);
            resp["reload_error"] = json!(e);
        }
    }
    (StatusCode::OK, Json(resp))
}

/// POST /api/plugins/new {name, description?} — scaffold a plugin.
async fn plugin_new(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = body["name"].as_str().unwrap_or("");
    let desc = body["description"].as_str().unwrap_or("");
    match veles_core::plugins::scaffold(name, desc) {
        Ok(dir) => {
            let entry = dir.join("plugin.tcl");
            let _ = srv.hooks.reload_plugins().await;
            (
                StatusCode::CREATED,
                Json(json!({
                    "name": name,
                    "dir": dir.display().to_string(),
                    // The UI opens this in the Dev editor straight away —
                    // a scaffold you then have to go and find is half a
                    // feature.
                    "path": entry.display().to_string(),
                })),
            )
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/plugins/install {url, name?} — clone a plugin repo.
async fn plugin_install(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let url = body["url"].as_str().unwrap_or("").trim();
    if url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no url supplied" })),
        );
    }
    let as_name = body["name"].as_str().filter(|s| !s.trim().is_empty());
    match veles_core::plugins::install(url, as_name) {
        Ok(dir) => {
            let mut resp = json!({
                "installed": dir.display().to_string(),
                "path": dir.join("plugin.tcl").display().to_string(),
            });
            match srv.hooks.reload_plugins().await {
                Ok(()) => resp["reloaded"] = json!(true),
                Err(e) => {
                    resp["reloaded"] = json!(false);
                    resp["reload_error"] = json!(e);
                }
            }
            (StatusCode::CREATED, Json(resp))
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// DELETE /api/plugins/:name — remove a plugin directory.
///
/// The plugins directory is a git repo the runtime snapshots on every
/// change, and `plugins::remove` commits before and after — so this is
/// undoable from the history list rather than final.
async fn plugin_delete(
    State(srv): State<Arc<DesktopServer>>,
    Path(name): Path<String>,
) -> (StatusCode, Json<Value>) {
    match veles_core::plugins::remove(&name) {
        Ok(dir) => {
            let _ = srv.hooks.reload_plugins().await;
            (
                StatusCode::OK,
                Json(json!({
                    "removed": dir.display().to_string(),
                    "note": "recoverable from the plugin history (git)",
                })),
            )
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/plugins/history — the plugin directory's git log.
async fn plugin_history() -> (StatusCode, Json<Value>) {
    match veles_core::plugins::history(50) {
        Ok(commits) => (
            StatusCode::OK,
            Json(json!({ "commits": commits, "count": commits.len() })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/plugins/rollback {commit} — restore the tree.
async fn plugin_rollback(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let commit = body["commit"].as_str().unwrap_or("");
    match veles_core::plugins::rollback(commit) {
        Ok(()) => {
            let mut resp = json!({ "rolled_back_to": commit });
            match srv.hooks.reload_plugins().await {
                Ok(()) => resp["reloaded"] = json!(true),
                Err(e) => {
                    resp["reloaded"] = json!(false);
                    resp["reload_error"] = json!(e);
                }
            }
            (StatusCode::OK, Json(resp))
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/cron — jobs with schedule/state.
async fn cron_list(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let jobs = srv
        .read("desktop::cron", |s| s.cron_list(false))
        .unwrap_or_default();
    let out: Vec<Value> = jobs.into_iter().map(cron_to_json).collect();
    Json(json!({ "jobs": out }))
}

fn cron_to_json(j: veles_core::store::CronJob) -> Value {
    json!({
        "id": j.id,
        "name": j.name,
        "schedule": j.schedule,
        // The normalized schedule is a machine string (`every:3600`);
        // an editor that shows only that makes the user decode their own
        // job. `describe_schedule` is what the CLI prints.
        "schedule_human": veles_core::cron::describe_schedule(&j.schedule),
        "prompt": j.prompt,
        "deliver": j.deliver,
        // The session a wakeup job resumes (Stage 5); null = fresh
        // session per run. The overlay shows it so a job's target is
        // visible without opening the run history.
        "session": j.session,
        "enabled": j.enabled,
        "no_agent": j.no_agent,
        "sandbox": j.sandbox,
        "script": j.script,
        "skills": j.skills,
        "model": j.model,
        "provider": j.provider,
        "reasoning": j.reasoning,
        "repeat_max": j.repeat_max,
        "run_count": j.run_count,
        "last_run_at": j.last_run_at,
        "last_status": j.last_status,
        "next_run_at": j.next_run_at,
        "created_at": j.created_at,
    })
}

/// POST /api/cron/new — create a job.
///
/// The store has had `cron_upsert`/`cron_update`/`cron_delete` all
/// along; only the agent's `cronjob` tool could reach them, so the Cron
/// overlay could pause and run jobs it had no way to author.
///
/// The schedule is NORMALIZED before it is stored, so a typo is refused
/// here rather than silently producing a job that never fires.
async fn cron_new(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let prompt = body["prompt"].as_str().unwrap_or("").trim().to_string();
    let script = body["script"].as_str().filter(|s| !s.trim().is_empty());
    if prompt.is_empty() && script.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a job needs a prompt (or a script)" })),
        );
    }
    let schedule =
        match veles_core::cron::normalize_schedule(body["schedule"].as_str().unwrap_or("")) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("{e:#}") })),
                )
            }
        };
    // Session-wakeup target (Stage 5), resolved NOW so a typo or an
    // ambiguous prefix is refused while the user is filling the form.
    let no_agent = body["no_agent"].as_bool().unwrap_or(false);
    let session = match body["session"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => {
            if no_agent {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({ "error": "session only applies to agent jobs (a script job has no session to wake)" }),
                    ),
                );
            }
            match veles_core::cron::resolve_session(&srv.store.lock_ok(), raw) {
                Ok(sid) => Some(sid),
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("{e:#}") })),
                    )
                }
            }
        }
        None => None,
    };
    let job = veles_core::store::CronJob {
        id: uuid::Uuid::new_v4().to_string()[..12].to_string(),
        name: body["name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(String::from),
        schedule,
        prompt,
        // The desktop has no platform origin, so "local" is the only
        // honest default — `origin` delivery would have nowhere to go.
        deliver: body["deliver"].as_str().unwrap_or("local").to_string(),
        enabled: true,
        script: script.map(String::from),
        skills: None,
        no_agent,
        sandbox: body["sandbox"].as_bool().unwrap_or(false),
        model: body["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        provider: body["provider"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        reasoning: body["reasoning"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        origin: None,
        session,
        repeat_max: body["repeat_max"].as_i64(),
        ..Default::default()
    };
    if let Err(e) = srv.store.lock_ok().cron_upsert(&job) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        );
    }
    // Compute and store the first fire, so the row shows when it will
    // actually run instead of a blank until the scheduler wakes.
    let next = veles_core::cron::next_fire(&job.schedule, &job, chrono::Utc::now())
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());
    if let Some(n) = &next {
        let _ = srv.store.lock_ok().cron_set_next(&job.id, Some(n));
    }
    let mut shown = job.clone();
    shown.next_run_at = next;
    (StatusCode::CREATED, Json(cron_to_json(shown)))
}

/// POST /api/cron/:id — edit a job. Absent fields are left alone.
async fn cron_edit(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    // Validate the schedule BEFORE touching the row: half an edit that
    // leaves a job with an unparseable schedule is worse than none.
    let schedule = match body["schedule"].as_str().filter(|s| !s.trim().is_empty()) {
        None => None,
        Some(raw) => match veles_core::cron::normalize_schedule(raw) {
            Ok(s) => Some(s),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("{e:#}") })),
                )
            }
        },
    };
    // `Option<Option<&str>>`: outer None = leave it, inner None = clear.
    let nullable = |key: &str| -> Option<Option<String>> {
        match body.get(key) {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => Some(v.as_str().filter(|s| !s.is_empty()).map(String::from)),
        }
    };
    let script = nullable("script");
    let model = nullable("model");
    let provider = nullable("provider");
    let reasoning = nullable("reasoning");
    // `session`: resolve at edit time like creation (a typo is refused
    // now, not at 03:00). null/empty clears it back to fresh-session
    // runs. Resolved to a FULL id, so the stored value is unambiguous.
    let session = match nullable("session") {
        None => None,
        Some(None) => Some(None),
        Some(Some(raw)) => match veles_core::cron::resolve_session(&srv.store.lock_ok(), &raw) {
            Ok(sid) => Some(Some(sid)),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("{e:#}") })),
                )
            }
        },
    };
    let repeat_max = match body.get("repeat_max") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => Some(v.as_i64()),
    };
    // Desktop edits keep the existing pin unless the payload says
    // otherwise — the cronjob tool's node= semantics, minus the
    // name-prefix resolution (the desktop has the id from its own list).
    let node = match body.get("node") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => v.as_str().map(|s| Some(s.to_string())),
    };

    let updated = srv.store.lock_ok().cron_update(
        &id,
        body["name"].as_str(),
        schedule.as_deref(),
        body["prompt"].as_str(),
        body["deliver"].as_str(),
        script.as_ref().map(|o| o.as_deref()),
        None,
        model.as_ref().map(|o| o.as_deref()),
        provider.as_ref().map(|o| o.as_deref()),
        reasoning.as_ref().map(|o| o.as_deref()),
        session.as_ref().map(|o| o.as_deref()),
        repeat_max,
        node.as_ref().map(|o| o.as_deref()),
    );
    match updated {
        Ok(Some(job)) => {
            // A changed schedule invalidates the stored next fire.
            if schedule.is_some() {
                let next = veles_core::cron::next_fire(&job.schedule, &job, chrono::Utc::now())
                    .ok()
                    .flatten()
                    .map(|t| t.to_rfc3339());
                let _ = srv.store.lock_ok().cron_set_next(&id, next.as_deref());
                let mut shown = job.clone();
                shown.next_run_at = next;
                return (StatusCode::OK, Json(cron_to_json(shown)));
            }
            (StatusCode::OK, Json(cron_to_json(job)))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no such job: {id}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// DELETE /api/cron/:id — remove a job and its run history.
async fn cron_delete(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match srv.store.lock_ok().cron_delete(&id) {
        Ok(true) => (StatusCode::OK, Json(json!({ "deleted": id }))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no such job: {id}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/cron/describe?schedule=... — what a schedule string means.
///
/// The create dialog echoes this back before saving: `every:3600` and
/// `cron:0 9 * * 1` are both easy to get subtly wrong, and finding out
/// by waiting is a bad feedback loop.
async fn cron_describe(Query(p): Query<StdMap<String, String>>) -> (StatusCode, Json<Value>) {
    let raw = p.get("schedule").map(String::as_str).unwrap_or("");
    match veles_core::cron::normalize_schedule(raw) {
        Ok(norm) => {
            let human = veles_core::cron::describe_schedule(&norm);
            (
                StatusCode::OK,
                Json(json!({ "schedule": norm, "human": human, "valid": true })),
            )
        }
        Err(e) => (
            StatusCode::OK,
            Json(json!({ "valid": false, "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/cron/:id/history — recent runs.
async fn cron_history(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let runs = srv
        .read("desktop::cron", |s| s.cron_run_history(&id, 20))
        .unwrap_or_default();
    let out: Vec<Value> = runs
        .into_iter()
        .map(|r| {
            json!({
                "run_id": r.run_id,
                "started_at": r.started_at,
                "finished_at": r.finished_at,
                "status": r.status,
                "session_id": r.session_id,
                "detail": r.detail,
            })
        })
        .collect();
    Json(json!({ "runs": out }))
}

/// POST /api/cron/:id/pause | /resume — enable flip.
async fn cron_pause(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match srv.store.lock_ok().cron_set_enabled(&id, false) {
        Ok(true) => (StatusCode::OK, Json(json!({ "paused": id }))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "job not found" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

async fn cron_resume(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match srv.store.lock_ok().cron_set_enabled(&id, true) {
        Ok(true) => (StatusCode::OK, Json(json!({ "resumed": id }))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "job not found" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/cron/:id/run — manual one-shot run (hermes `cronjob run`).
/// Spawns and returns immediately; poll /api/cron/:id/history for the
/// outcome (agent jobs take LLM time).
async fn cron_run(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let sched = match &srv.scheduler {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "scheduler not attached (desktop running standalone?)" })),
            )
        }
    };
    let job = match srv.store.lock_ok().cron_get(&id) {
        Ok(Some(j)) => j,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "job not found" })),
            )
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    let id2 = id.clone();
    veles_core::rt::spawn(async move {
        let summary = sched.run_job_manual(&job).await;
        tracing::info!("manual cron run {id2}: {summary}");
    });
    (StatusCode::ACCEPTED, Json(json!({ "started": id })))
}

// ── /api/plugins/:name/<route> — a module's own routes (R5) ─────────
//
// A module serves its desktop surface itself (`moduleroutes`): the
// slopdrop mirror's status/sync/save are its, under
// `/api/plugins/slopdrop/{status,sync,save}`. The API owns the listener
// and the bearer check; the module owns the answer. A name no loaded
// module serves is 503 with the reason, so a pane fails closed and says
// why. The old `/api/slopdrop*` spellings stay as aliases for the
// desktops already installed.

fn module_request(
    method: &axum::http::Method,
    route: &str,
    query: std::collections::BTreeMap<String, String>,
    body: Option<Value>,
) -> veles_core::moduleroutes::RouteRequest {
    veles_core::moduleroutes::RouteRequest {
        method: method.as_str().to_ascii_uppercase(),
        path: format!("/{}", route.trim_start_matches('/')),
        query,
        body: body.unwrap_or(Value::Null),
        bearer: None,
    }
}

/// Is this a module's OPEN route, `/api/plugins/<name>/open/<route>`?
/// Exactly that shape: the bearer gate is skipped for nothing else.
fn is_module_open_route(path: &str) -> bool {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    matches!(segs.as_slice(), ["api", "plugins", name, "open", route]
        if !name.is_empty() && !route.is_empty())
}

/// `/api/plugins/:name/open/:route` — a module route OUTSIDE the bearer
/// gate (`moduleroutes::ModuleRoutes::handle_open`): the module checks a
/// credential of its own, which arrives as the request's bearer. The
/// slopdrop module's Bluesky reads (a per-evaluation token) are the
/// first; a module that does not implement it answers 404.
async fn module_open_route(
    Path((name, route)): Path<(String, String)>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::BTreeMap<String, String>>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let mut req = module_request(
        &method,
        &route,
        query,
        serde_json::from_slice::<Value>(&body).ok(),
    );
    req.bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let r = match veles_core::moduleroutes::dispatch_open(&name, req).await {
        Some(r) => r,
        None => veles_core::moduleroutes::not_loaded(&name),
    };
    (
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(r.body),
    )
}

async fn module_answer(
    name: &str,
    req: veles_core::moduleroutes::RouteRequest,
) -> (StatusCode, Json<Value>) {
    let r = match veles_core::moduleroutes::dispatch(name, req).await {
        Some(r) => r,
        None => veles_core::moduleroutes::not_loaded(name),
    };
    (
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(r.body),
    )
}

/// GET /api/plugins/:name/:route — a module's read route.
async fn module_route_get(
    Path((name, route)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<std::collections::BTreeMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    module_answer(
        &name,
        module_request(&axum::http::Method::GET, &route, query, None),
    )
    .await
}

/// POST /api/plugins/:name/:route with a JSON body — a module's write
/// route (`enable`/`disable` are the plugin switch, `plugin_set_enabled`).
async fn module_route_post(
    name: &str,
    route: &str,
    body: Option<Value>,
) -> (StatusCode, Json<Value>) {
    module_answer(
        name,
        module_request(
            &axum::http::Method::POST,
            route,
            std::collections::BTreeMap::new(),
            body,
        ),
    )
    .await
}

/// GET /api/slopdrop — alias of /api/plugins/slopdrop/status.
async fn slopdrop_status_alias() -> (StatusCode, Json<Value>) {
    module_answer(
        "slopdrop",
        module_request(
            &axum::http::Method::GET,
            "status",
            std::collections::BTreeMap::new(),
            None,
        ),
    )
    .await
}

/// POST /api/slopdrop/sync — alias of /api/plugins/slopdrop/sync.
async fn slopdrop_sync_alias() -> (StatusCode, Json<Value>) {
    module_route_post("slopdrop", "sync", None).await
}

/// POST /api/slopdrop/save — alias of /api/plugins/slopdrop/save.
async fn slopdrop_save_alias(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    module_route_post("slopdrop", "save", Some(body)).await
}

// ── /api/triggers — the trigger table, same shape as /api/cron ──────
//
// The desktop gets the store view; the ENGINE lives in the IRC
// adapter's process, and the derived forward masks in slopdrop's
// answer. This table is rows + fire history, and the `/triggers
// refresh`/`mask` operations run through the shared command
// dispatcher (POST /api/command) — the same seam every other
// surface's trigger words use.

/// GET /api/triggers — every row, structured for a table editor.
async fn triggers_list(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let rows = srv
        .read("desktop::triggers", |s| s.trigger_list(false))
        .unwrap_or_default();
    let out: Vec<Value> = rows.into_iter().map(trigger_to_json).collect();
    Json(json!({ "triggers": out }))
}

fn trigger_to_json(t: veles_core::store::TriggerRow) -> Value {
    json!({
        "id": t.id,
        "name": t.name,
        "enabled": t.enabled,
        "platform": t.platform,
        "network": t.network,
        "channel": t.channel,
        "event": t.event,
        "match_kind": t.match_kind,
        "pattern": t.pattern,
        "action": t.action,
        "arg": t.arg,
        "cooldown_secs": t.cooldown_secs,
        "per_hour_cap": t.per_hour_cap,
        "min_role": t.min_role,
        "fire_count": t.fire_count,
        "last_fired_at": t.last_fired_at,
        "last_status": t.last_status,
    })
}

/// POST /api/triggers/new — create a row. The k=v surface shares its
/// validation with the chat command's (`triggers_command` rejects on
/// unknown event/action/match), but a structured editor validates here:
/// an invalid row must never enter the store.
async fn trigger_new(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a trigger needs a name" })),
        );
    }
    // The SHARED validators (`veles_core::triggers`), not a fourth private
    // copy: this surface checked event, action and match with its own
    // spelling of each and `min_role` with none at all, while the chat
    // surfaces checked a different subset again. A typo'd floor reads
    // back as Guest, which every sender that reaches the hook is
    // already at or above — the widening direction.
    let event = match veles_core::triggers::validate_event(body["event"].as_str().unwrap_or("TEXT"))
    {
        Ok(e) => e,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    };
    let action = match veles_core::triggers::validate_action(
        body["action"].as_str().unwrap_or("slopdrop"),
    ) {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    };
    let match_kind =
        match veles_core::triggers::validate_match(body["match_kind"].as_str().unwrap_or("always"))
        {
            Ok(m) => m,
            Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
        };
    let min_role = match body["min_role"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => match veles_core::triggers::validate_min_role(r) {
            Ok(r) => Some(r),
            Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
        },
        None => None,
    };
    let row = veles_core::store::TriggerRow {
        id: 0,
        name,
        enabled: true,
        platform: body["platform"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or(Some(veles_core::triggers::default_platform(&event).into())),
        network: body["network"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        channel: body["channel"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        event,
        match_kind,
        pattern: body["pattern"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        action,
        arg: body["arg"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        cooldown_secs: body["cooldown_secs"].as_i64().unwrap_or(0),
        per_hour_cap: body["per_hour_cap"].as_i64().unwrap_or(0),
        min_role,
        fire_count: 0,
        last_fired_at: None,
        last_status: None,
    };
    // ONE lock for the whole handler. The earlier shape put `lock_ok()`
    // in the `match` scrutinee and locked AGAIN in the `Ok` arm; the
    // scrutinee guard is a temporary that lives until the end of the
    // match (2021 temporary-scope rule), so the second lock was a
    // self-deadlock on a non-reentrant mutex — `trigger_upsert` had
    // already committed but the handler never returned, and every
    // surface shares this one store mutex, so the gateway froze. Bind
    // both results under a single guard, then match on the pair.
    let (upserted, shown) = {
        let s = srv.store.lock_ok();
        let up = s.trigger_upsert(&row);
        let got = match &up {
            Ok(_) => s.trigger_get(&row.name).ok().flatten(),
            Err(_) => None,
        };
        (up, got)
    };
    match upserted {
        Ok(id) => match shown {
            Some(s) => (StatusCode::CREATED, Json(trigger_to_json(s))),
            None => (
                StatusCode::CREATED,
                Json(json!({ "name": row.name, "id": id })),
            ),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/triggers/:name — edit a row by name. Null clears a
/// nullable field; absent leaves it alone.
async fn trigger_edit(
    State(srv): State<Arc<DesktopServer>>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let nullable = |key: &str| -> Option<Option<String>> {
        match body.get(key) {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => Some(v.as_str().filter(|s| !s.is_empty()).map(String::from)),
        }
    };
    let arg = nullable("arg");
    let pattern = nullable("pattern");
    let cooldown_secs = body.get("cooldown_secs").map(|v| v.as_i64().unwrap_or(0));
    let per_hour_cap = body.get("per_hour_cap").map(|v| v.as_i64().unwrap_or(0));
    // Event was the only field this arm checked. `match_kind`, `action`
    // and `min_role` went straight to the store, so the desktop's edit
    // dialog could write a row the engine reads back through
    // `into_trigger`'s widening defaults. Same validators as every
    // other surface; a bad value is a 400, not a silently broader row.
    let mut bad: Option<String> = None;
    let mut canon = |key: &str, f: fn(&str) -> Result<String, String>| -> Option<String> {
        let raw = body.get(key)?.as_str().filter(|s| !s.is_empty())?;
        match f(raw) {
            Ok(v) => Some(v),
            Err(e) => {
                bad = Some(e);
                None
            }
        }
    };
    let event = canon("event", veles_core::triggers::validate_event);
    let match_kind = canon("match_kind", veles_core::triggers::validate_match);
    let action = canon("action", veles_core::triggers::validate_action);
    let min_role_set = canon("min_role", veles_core::triggers::validate_min_role);
    if let Some(e) = bad {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    }
    // `null` still CLEARS the floor; only a non-empty value is validated.
    let min_role = match body.get("min_role") {
        Some(Value::Null) => Some(None),
        Some(_) => min_role_set.map(Some),
        None => None,
    };

    let updated = srv.store.lock_ok().trigger_update(
        &name,
        nullable("network").flatten().as_deref(),
        nullable("channel").flatten().as_deref(),
        event.as_deref(),
        match_kind.as_deref(),
        pattern.as_ref().map(|o| o.as_deref()),
        action.as_deref(),
        arg.as_ref().map(|o| o.as_deref()),
        cooldown_secs,
        per_hour_cap,
        min_role.as_ref().map(|o| o.as_deref()),
    );
    match updated {
        Ok(Some(row)) => (StatusCode::OK, Json(trigger_to_json(row))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no such trigger: {name}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// DELETE /api/triggers/:name — remove a row and its fire history.
async fn trigger_delete(
    State(srv): State<Arc<DesktopServer>>,
    Path(name): Path<String>,
) -> (StatusCode, Json<Value>) {
    match srv.store.lock_ok().trigger_delete(&name) {
        Ok(true) => (StatusCode::OK, Json(json!({ "deleted": name }))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such trigger" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/triggers/:name/enable | /disable — flip a row.
async fn trigger_set_enabled(
    State(srv): State<Arc<DesktopServer>>,
    Path((name, verb)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    let enabled = verb == "enable";
    match srv.store.lock_ok().trigger_set_enabled(&name, enabled) {
        Ok(true) => (
            StatusCode::OK,
            Json(json!({ "name": name, "enabled": enabled })),
        ),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such trigger" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// GET /api/triggers/:name/history — recent fires and refusals.
async fn trigger_history(
    State(srv): State<Arc<DesktopServer>>,
    Path(name): Path<String>,
) -> Json<Value> {
    let fires = srv
        .read("desktop::triggers", |s| {
            s.trigger_fire_history(Some(&name), 20)
        })
        .unwrap_or_default();
    let out: Vec<Value> = fires
        .into_iter()
        .map(|f| {
            json!({
                "fire_id": f.fire_id,
                "name": f.name,
                "channel": f.channel,
                "at": f.at,
                "status": f.status,
                "detail": f.detail,
            })
        })
        .collect();
    Json(json!({ "fires": out }))
}

/// POST /api/shells — start a background shell.
///
/// The HTTP twin of `/background <cmd>` (same capability: the model
/// reaches it through the `terminal` tool, this serves every other
/// client), so a background command is startable from wherever the user
/// is rather than only from inside a turn.
async fn shell_start(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let command = body["command"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if command.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "command required" })),
        );
    }
    let session = body["session"].as_str().unwrap_or("api").to_string();
    // The desktop is an owner surface (no principal on these sessions)
    // — host zone unless the body explicitly asks for the sandbox env.
    let id = veles_core::shells::start(
        &command,
        &session,
        veles_core::user_envs::ZoneArg::from_args(&body),
    );
    (StatusCode::OK, Json(json!({ "task_id": id })))
}

/// POST /api/btw — start a one-off subagent for a session.
///
/// The HTTP twin of the `/btw` command (was `/api/background` before
/// the taxonomy split, USER 2026-09-17 — `/background` now starts
/// SHELLS, which is what `/api/shells` has always been for, so the
/// subagent spawn needed its own honest name). Same tracked child the
/// command starts, through the same `spawn_background_child` — including
/// the fork: the child session inherits the parent's history and shows
/// up in the session list (↳) the moment it is minted.
async fn btw_start(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let session = body["session"].as_str().unwrap_or_default().to_string();
    let prompt = body["prompt"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "prompt required" })),
        );
    }
    if session.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "session required" })),
        );
    }
    let id = veles_core::delegate::spawn_background_child(&prompt, &session);
    (StatusCode::OK, Json(json!({ "task_id": id })))
}

/// GET /api/tasks — running subagents, background shells and monitors.
///
/// The first surface to read the task registry at all. Its predecessor
/// (`SubagentRegistry`) was written on every delegate call and read by
/// nothing for the life of the feature.
async fn tasks_list() -> Json<Value> {
    let mut v = veles_core::tasks::snapshot();
    // The same text every other frontend prints, so a desktop user and a
    // terminal user are never looking at differently-worded state.
    v["listing"] = json!(veles_core::tasks::listing());
    Json(v)
}

/// GET /api/tasks/:id/output — captured output for a background shell.
async fn task_output(Path(id): Path<u32>) -> (StatusCode, Json<Value>) {
    match veles_core::shells::detail(id) {
        Some(v) => (StatusCode::OK, Json(v)),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such shell", "id": id})),
        ),
    }
}

/// POST /api/tasks/:id/cancel — stop one task.
async fn task_cancel(Path(id): Path<u32>) -> (StatusCode, Json<Value>) {
    if veles_core::tasks::cancel(id) {
        (StatusCode::OK, Json(json!({"cancelled": id})))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown task, or it already finished", "id": id})),
        )
    }
}

/// POST /api/sessions/:id/cancel — stop the session's in-flight turn.
///
/// The EXPLICIT replacement for cancel-on-disconnect: the Stop button
/// and any second window come here instead of relying on the SSE
/// stream's lifetime.
///
/// Delegates to `agent::cancel_turn`, which flips the token in the ONE
/// process-wide registry (`LIVE_TURNS`) that `run_turn` populates for
/// every surface. This endpoint used to walk a desktop-only `TurnCancels`
/// map, so it could not cancel — or even see — a turn started by IRC,
/// telegram or cron, and it disagreed with `agent::cancel_turn` about
/// whether the same session was running. One registry, one answer.
async fn session_cancel(Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    if veles_core::agent::cancel_turn(&id) {
        (
            StatusCode::OK,
            Json(json!({"cancelling": 1, "session": id})),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no turn in flight", "id": id})),
        )
    }
}

/// POST /api/sessions/:id/detach — move the session's running
/// foreground `terminal` command into the background shells.
///
/// The gentle sibling of [`session_cancel`], same seam: an explicit
/// endpoint (not a flag smuggled through the chat lock) because the
/// turn holding the agent lock is the very turn blocking on the
/// command. Arms the one-shot flag `agent::request_detach`; the tool's
/// wait loop adopts the live process as a shell row within one poll
/// (~100ms) and its tool result names the task id.
///
/// Three honest outcomes, same wording as the chat command so the
/// surfaces cannot disagree: detached / turn running but not inside a
/// foreground command / nothing running at all.
async fn session_detach(Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    if veles_core::agent::request_detach(&id) {
        (
            StatusCode::OK,
            Json(
                json!({"detaching": 1, "session": id, "note": "the command keeps running as a background shell; its task id lands in the turn"}),
            ),
        )
    } else if veles_core::agent::turn_is_live(&id) {
        (
            StatusCode::CONFLICT,
            Json(
                json!({"error": "the running turn is not inside a foreground terminal command right now", "session": id}),
            ),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no turn in flight", "id": id})),
        )
    }
}

/// GET /api/agents — list defined subagents (S7).
async fn agents_list() -> Json<Value> {
    let defs = veles_core::agents::discover();
    Json(serde_json::json!({
        "listing": veles_core::agents::listing(),
        "agents": defs.iter().map(|a| serde_json::json!({
            "name": a.name,
            "description": a.description,
            "model": a.model,
            "provider": a.provider,
            "reasoning": a.reasoning,
            "source": a.source.display().to_string(),
        })).collect::<Vec<_>>(),
    }))
}

/// GET /api/commands — list custom slash commands (S6): both the .md
/// files on disk and the commands plugins registered at runtime.
async fn commands_list() -> Json<Value> {
    let disk = veles_core::commands::discover();
    let tcl = veles_core::commands::plugin_commands();
    let mut entries: Vec<Value> = disk
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "description": c.description,
                "source": c.source.display().to_string(),
            })
        })
        .collect();
    entries.extend(tcl.iter().map(|c| {
        serde_json::json!({
            "name": c.name,
            "description": c.description,
            "source": format!("plugin {}", c.origin()),
        })
    }));
    entries.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    Json(Value::Object({
        let mut m = serde_json::Map::new();
        m.insert("commands".into(), Value::Array(entries));
        m
    }))
}

/// S6: render a custom command with args. Owner surface (desktop is
/// loopback-only), so shell/file expansion is trusted here.
/// Plugin-registered commands render through the plugins interpreter
/// (async hop — this handler runs inside the axum runtime).
async fn commands_render(Json(body): Json<Value>) -> Json<Value> {
    let name = body["name"].as_str().unwrap_or_default();
    let args = body["args"].as_str().unwrap_or_default();
    let rendered: Result<(String, String), String> = match veles_core::commands::find_any(name) {
        Some(veles_core::commands::ResolvedCommand::Disk(c)) => {
            Ok((veles_core::commands::render(&c, args), c.name.clone()))
        }
        Some(veles_core::commands::ResolvedCommand::Plugin(t)) => {
            veles_core::commands::render_plugin_async(&t, args)
                .await
                .map(|s| (s, t.name.clone()))
        }
        None => Err(format!("unknown command /{name}")),
    };
    match rendered {
        Ok((prompt, cname)) => Json(Value::Object({
            let mut m = serde_json::Map::new();
            m.insert("prompt".into(), Value::String(prompt));
            m.insert("name".into(), Value::String(cname));
            m
        })),
        Err(e) => Json(Value::Object({
            let mut m = serde_json::Map::new();
            m.insert("prompt".into(), Value::String(String::new()));
            m.insert("error".into(), Value::String(e));
            m
        })),
    }
}

async fn list_tools(State(srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let registry = veles_core::tools::builtin_registry(srv.store.clone(), None, "__tools_view__");
    let mut builtin = registry.names();
    builtin.sort();
    // Two different deferred sets, and this endpoint used to report only
    // one. `deferred::deferred_names()` is the PROCESS-GLOBAL registry
    // (MCP servers, adapter tools); `registry.deferred_names()` is the
    // SESSION registry's own map, which is where tool tiering puts every
    // builtin it moves out of the prompt. Reporting only the global one
    // made ~25 builtins vanish from the desktop's tool view the moment
    // tiering landed — present and callable, listed nowhere.
    //
    // Same invariant the cronjob recursion guard is tested against:
    // what a session can REACH is resident ∪ deferred.
    let mut deferred = registry.deferred_names();
    deferred.extend(veles_core::deferred::deferred_names());
    deferred.sort();
    deferred.dedup();
    Json(json!({
        "builtin": builtin,
        "deferred": deferred,
        "builtin_count": builtin.len(),
        "deferred_count": deferred.len(),
    }))
}

/// GET /api/usage — global token/message totals (hermes /usage parity).
#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default)]
    session: Option<String>,
}

/// GET /api/usage[?session=<id>] — token totals.
///
/// Without `session` these are LIFETIME totals across every session,
/// which is what the status bar was showing under a bare "tokens 11/22"
/// — indistinguishable from the current turn, and so read as wrong.
/// Both scopes are now returned, labelled, so a caller cannot show one
/// while meaning the other.
///
/// `by_plan` is the SPLIT (USER 2026-09-20: "the existing usage is
/// confusing, it conflates the personal and team alibaba tokens").
/// A single total across the personal plan, the team plan, z.ai and
/// Kimi is a number nobody can act on — the pools are disjoint and so
/// are the bills. `veles_core::tokenplan::ledger_by_plan` does the grouping
/// once so every surface renders the same split; providers no plan
/// names (openrouter, mistral, …) come back as their own rows rather
/// than being folded into a plan they do not belong to.
async fn usage(State(srv): State<Arc<DesktopServer>>, Query(q): Query<UsageQuery>) -> Json<Value> {
    // Off the async workers, on a pooled connection: the totals sum the
    // whole usage table, and the status bar asks on a timer.
    veles_core::rt::spawn_blocking(move || srv.read("desktop::usage", |store| usage_on(store, &q)))
        .await
        .unwrap_or_else(|e| Json(json!({ "error": format!("reading usage: {e}") })))
}

fn usage_on(store: &Store, q: &UsageQuery) -> Json<Value> {
    // usage_totals is a named struct now: the positional tuple it used
    // to be is how the status bar once reported the call count as the
    // prompt total.
    let u = store.usage_totals(None).unwrap_or_default();
    let session = q.session.as_deref().map(|sid| {
        let s = store.usage_totals(Some(sid)).unwrap_or_default();
        json!({
            "prompt_tokens": s.prompt_tokens,
            "completion_tokens": s.completion_tokens,
            "cached_tokens": s.cached_tokens,
            "cache_write_tokens": s.cache_write_tokens,
            "calls": s.calls,
        })
    });
    // Both scopes get a split. A failed query is an empty list, which
    // renders as "no plan rows" rather than as a wrong number.
    let split = |sid: Option<&str>| {
        let rows = store.usage_by_provider_scoped(sid).unwrap_or_default();
        veles_core::tokenplan::ledger_json(&veles_core::tokenplan::ledger_by_plan(&rows))
    };
    let by_plan = split(None);
    let session_by_plan = q.session.as_deref().map(|sid| {
        let rows = store
            .usage_by_provider_scoped(Some(sid))
            .unwrap_or_default();
        veles_core::tokenplan::ledger_json(&veles_core::tokenplan::ledger_by_plan(&rows))
    });
    Json(json!({
        "prompt_tokens": u.prompt_tokens,
        "completion_tokens": u.completion_tokens,
        "cached_tokens": u.cached_tokens,
        "cache_write_tokens": u.cache_write_tokens,
        "calls": u.calls,
        "scope": "all sessions",
        "session": session,
        "by_plan": by_plan,
        "session_by_plan": session_by_plan,
    }))
}

/// GET /api/quota[?refresh=1] — the UPSTREAM plan's quota.
///
/// CACHE-ONLY by default, and that is the contract, not an
/// optimisation: the status bar repaints on a 15s timer and the overlay
/// repaints whenever it is opened, so a handler that could reach
/// Singapore would put a foreign round trip on both. Only `refresh=1`
/// (the overlay's button, `/quota refresh`) goes out, and the gateway's
/// background warm keeps the cache fresh without anybody asking.
///
/// `age_secs` is served beside the figures so the UI can say how old
/// they are rather than implying they are live.
#[derive(Deserialize)]
struct QuotaQuery {
    #[serde(default)]
    refresh: Option<String>,
    /// The provider the caller is routing to. With it, the engine names
    /// the plan that provider draws from — `active_plan` and
    /// `active_plan_fraction` below.
    ///
    /// This exists so the mapping lives in ONE place. The desktop used
    /// to re-derive it with its own substring matching on the provider
    /// name, which meant two implementations of "which pool is this"
    /// that had to agree and had no way to notice when they did not —
    /// and the engine's own copy was WRONG for `ali-responses` (a team
    /// credential under a name containing neither "team" nor
    /// "personal"), so both were serving the personal pool's number to
    /// a session spending team credits.
    #[serde(default)]
    provider: Option<String>,
}

async fn quota(State(srv): State<Arc<DesktopServer>>, Query(q): Query<QuotaQuery>) -> Json<Value> {
    let cfg = srv.cfg();
    let cfg = cfg.as_ref();
    if !cfg.tokenplan.enabled {
        return Json(json!({
            "enabled": false,
            "reason": "[tokenplan] enabled = false in config.toml",
        }));
    }
    let force = matches!(q.refresh.as_deref(), Some("1") | Some("true"));
    let (snap, age) = if force {
        (
            veles_core::tokenplan::refresh(cfg, None).await,
            std::time::Duration::ZERO,
        )
    } else {
        // `for_display`, not `cached`: past the TTL a reading is OLD,
        // never absent. This used to fall straight from `cached` to the
        // cold answer below — and with the shipped defaults (ttl 300,
        // poll 900) a reading is past its TTL for 600 of every 900
        // seconds, so the desktop's plan field sat at "plan —" two
        // thirds of the time and looked like it had switched itself
        // off. The payload marks it `stale` and says how old it is.
        match veles_core::tokenplan::for_display(cfg) {
            Some(hit) => hit,
            // Nothing has EVER arrived — this is what cold means, and
            // now the only thing it means. Say so rather than silently
            // fetching on a paint path; the caller decides whether to
            // ask for a refresh.
            //
            // The plan resolution is still answered, because it depends
            // on the PROVIDER and not on any reading: a caller asking
            // "which pool am I on" gets the same answer cold as warm,
            // and only the fraction is missing.
            None => {
                let mut v = json!({
                    "enabled": true,
                    "cold": true,
                    "ttl_secs": cfg.tokenplan.cache_ttl_secs,
                    "poll_secs": cfg.tokenplan.poll_secs,
                });
                if let Some(p) = &q.provider {
                    v["active_plan"] =
                        json!(veles_core::tokenplan::plan_for_provider(p).map(|p| p.label()));
                }
                return Json(v);
            }
        }
    };
    let mut v = veles_core::tokenplan::snapshot_json(
        &snap,
        age,
        cfg.tokenplan.cache_ttl_secs,
        cfg.tokenplan.poll_secs,
    );
    // One mapping, in the engine, for every surface. Merging rather
    // than wrapping keeps the snapshot shape flat for existing readers.
    //
    // `plan_fractions` is merged unconditionally: a widget that lists
    // all four pools needs every fraction, and it needs them whether or
    // not the session has a route yet. `active_plan` /
    // `active_plan_fraction` are the provider-specific half and only
    // exist when a provider was named.
    let a = active_plan_json(&snap, q.provider.as_deref().unwrap_or(""));
    v["plan_fractions"] = a["plan_fractions"].clone();
    if q.provider.is_some() {
        v["active_plan"] = a["active_plan"].clone();
        v["active_plan_fraction"] = a["active_plan_fraction"].clone();
    }
    Json(v)
}

/// The plan a provider draws from, and what that plan has used.
///
/// Served from `/api/quota?provider=<route provider>` so the mapping
/// and the figure are resolved by the SAME code that owns the pool
/// definitions — the desktop renders what it is told rather than
/// guessing from a provider-name substring (see `QuotaQuery.provider`).
///
/// `active_plan` is `null` when no plan covers the provider — a real
/// answer, not a fallback: a session on OpenRouter is on no plan this
/// account reads, and the honest display for that is our own ledger
/// amount with no upstream percentage beside it.
pub fn active_plan_json(snap: &veles_core::tokenplan::QuotaSnapshot, provider: &str) -> Value {
    use veles_core::tokenplan::{plan_for_provider, Plan};
    let plan = plan_for_provider(provider);
    serde_json::json!({
        "active_plan": plan.map(|p| p.label()),
        "active_plan_fraction": plan.and_then(|p| p.tightest_used_fraction(snap)),
        // Every pool's fraction under its own name, so a caller that
        // wants to render all of them (a fresh session with no route,
        // say) does not have to reassemble the mapping.
        "plan_fractions": {
            "team": Plan::Team.tightest_used_fraction(snap),
            "personal": Plan::Personal.tightest_used_fraction(snap),
            "zai": Plan::Zai.tightest_used_fraction(snap),
            "kimi": Plan::Kimi.tightest_used_fraction(snap),
        },
    })
}

#[derive(Deserialize)]
struct TitleRequest {
    title: String,
}

/// POST /api/sessions/:id/title — rename (sidebar parity).
// ── the shared message queue (R4.6) ─────────────────────────────────
//
// One store table (`session_queue`), edited over REST here, pushed by
// the TUI mid-turn, drained at every surface's turn boundary. The row
// set IS the queue — there is nothing in-process to keep consistent,
// which is what makes "the TUI queued it and the desktop shows it"
// fall out of the store rather than out of socket plumbing.

#[derive(Deserialize)]
struct QueuePushRequest {
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct QueueMutateRequest {
    seq: i64,
    #[serde(default)]
    body: Option<String>,
}

/// The wire shape of one queue row. Flat JSON: the Tcl overlay reads
/// this directly, and the seq is the handle its edit/cancel buttons
/// POST back.
fn queue_items_json(srv: &DesktopServer, id: &str) -> serde_json::Value {
    json!({
        "items": srv
            .store
            .lock_ok()
            .queue_items(id)
            .unwrap_or_default()
            .iter()
            .map(|it| json!({
                "session_id": it.session_id,
                "seq": it.seq,
                "body": it.body,
                "state": it.state,
                "queued_at": it.queued_at,
            }))
            .collect::<Vec<_>>()
    })
}

/// Push a `queue-changed` event on the session bus, so the desktop's
/// open session stream redraws its queue panel without polling.
fn publish_queue_changed(srv: &DesktopServer, id: &str) {
    let _ = veles_core::session_tap::publish(id, "queue-changed", queue_items_json(srv, id));
}

/// GET /api/sessions/:id/queue — the whole queue (pending + running),
/// oldest first. No side effects; no turn to run.
async fn queue_view(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(queue_items_json(&srv, &id)))
}

/// POST /api/sessions/:id/queue {"body": …} — park a message. Called
/// both explicitly (the desktop's "queue" affordance) and by
/// `POST /api/chat` when a turn is already live.
async fn queue_push(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(req): Json<QueuePushRequest>,
) -> (StatusCode, Json<Value>) {
    if req.body.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "queue body must not be empty" })),
        );
    }
    if !srv.store.lock_ok().session_exists(&id).unwrap_or(false) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        );
    }
    let pushed = srv.store.lock_ok().queue_push(&id, req.body.trim());
    match pushed {
        Ok(seq) => {
            publish_queue_changed(&srv, &id);
            (StatusCode::OK, Json(json!({ "queued": true, "seq": seq })))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// PATCH /api/sessions/:id/queue {"seq": n, "body": …} — rewrite a
/// queued message before it becomes a turn. Pending rows only; a
/// message already running is the live turn, and editing THAT is what
/// /steer means.
async fn queue_patch(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(req): Json<QueueMutateRequest>,
) -> (StatusCode, Json<Value>) {
    let body = req.body.clone().unwrap_or_default();
    let patched = srv.store.lock_ok().queue_patch(&id, req.seq, body.trim());
    match patched {
        Ok(true) => {
            publish_queue_changed(&srv, &id);
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no pending message #{}", req.seq) })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// DELETE /api/sessions/:id/queue {"seq": n} — cancel a queued
/// message. The running turn is not a queue row's problem; that is
/// still /cancel.
async fn queue_delete(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(req): Json<QueueMutateRequest>,
) -> (StatusCode, Json<Value>) {
    let deleted = srv.store.lock_ok().queue_delete(&id, req.seq);
    match deleted {
        Ok(true) => {
            publish_queue_changed(&srv, &id);
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no pending message #{}", req.seq) })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

async fn set_title(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(req): Json<TitleRequest>,
) -> (StatusCode, Json<Value>) {
    if !srv.store.lock_ok().session_exists(&id).unwrap_or(false) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        );
    }
    match srv.store.lock_ok().set_title(&id, &req.title) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

#[derive(Deserialize, Default)]
struct OverrideRequest {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

/// GET /api/sessions/:id/override — what this session will ACTUALLY
/// send, with the deferred (not-yet-applied) choice if there is one.
///
/// Without this the overlay could only echo the session id back, so a
/// setting that silently failed to apply looked exactly like one that
/// worked.
async fn get_override(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let cached = srv.agents.lock_ok().get(&id).cloned();
    if let Some(a) = cached {
        let agent = &a;
        let (provider, model, reasoning, overridden) = agent.effective_model();
        return (
            StatusCode::OK,
            Json(json!({
                "live": true,
                "provider": provider,
                "model": model,
                "reasoning": reasoning,
                "overridden": overridden,
            })),
        );
    }
    // Not started yet: report the stored choice, clearly labelled as
    // pending rather than in effect. The store row is the ONE home
    // (R4.5) — desktop_state.json's `override:<sid>` keys are migrated
    // on sight below and read nowhere.
    let pending = srv
        .store
        .lock_ok()
        .override_get(&id)
        .ok()
        .flatten()
        .map(|o| {
            json!({
                "provider": o.provider,
                "model": o.model,
                "reasoning": o.reasoning,
            })
        })
        .unwrap_or(Value::Null);
    (
        StatusCode::OK,
        Json(json!({ "live": false, "pending": pending })),
    )
}

/// POST /api/sessions/:id/override — session-scoped model/provider/
/// reasoning. Applies live when the agent is cached; recorded in the
/// desktop state blob otherwise (applied lazily — the desktop's own
/// agents come from this server's cache, so a later chat will see it).
async fn set_override(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(req): Json<OverrideRequest>,
) -> (StatusCode, Json<Value>) {
    // The blob is the DURABLE home for an override; the live agent is
    // the fast path. The old code wrote desktop_state.json ONLY on the
    // deferred branch, so a LIVE override died with the process — every
    // gateway restart this session silently reverted sessions to the
    // default ("sometimes it forgot the override"). Persist the raw
    // request EVERY time (the first-use consumer re-resolves aliases,
    // exactly like the live path), and DELETE the key on a full clear.
    //
    // Check the session is real FIRST (and persists under a scratch
    // home in tests), then write, then mutate.
    match srv.store.lock_ok().session_exists(&id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("session not found: {id}") })),
            )
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    }
    // A "clear" is all three fields EXPLICITLY empty (the UI's clear
    // button sends {"model":"","provider":"","reasoning":""}). None is
    // "not specified / leave alone" — an all-None request is a no-op,
    // NOT a clear.
    let fully_cleared = req.model.as_deref().is_some_and(|m| m.is_empty())
        && req.provider.as_deref().is_some_and(|p| p.is_empty())
        && req.reasoning.as_deref().is_some_and(|r| r.is_empty());
    // The store row is the durable home (R4.5): one fact every surface
    // reads, applied by the next agent to open the session. The old
    // desktop_state.json blob was invisible to the TUI, IRC, Telegram
    // and the gateway, and died with the desktop's own cache. Fields
    // left as None keep what the row had — None means "leave alone"
    // here, mirroring the live path below.
    {
        let st = srv.store.lock_ok();
        if fully_cleared {
            let _ = st.override_replace(&id, None, None, None);
        } else {
            let pick = |new: &Option<String>, old: Option<String>| match new {
                Some(s) if !s.is_empty() => Some(s.clone()),
                Some(_) => None, // explicit empty clears one field
                None => old,
            };
            let cur = st.override_get(&id).ok().flatten().unwrap_or_default();
            let (p0, m0, r0) = (cur.provider, cur.model, cur.reasoning);
            let _ = st.override_replace(
                &id,
                pick(&req.provider, p0).as_deref(),
                pick(&req.model, m0).as_deref(),
                pick(&req.reasoning, r0).as_deref(),
            );
        }
    }

    // Live agent: mutate in place (hermes /model is session-scoped).
    // The lock_ok() guard must DROP before the `.await` below — holding
    // a std::sync::MutexGuard across an await makes this future
    // !Send and axum rejects it as a handler.
    let cached = srv.agents.lock_ok().get(&id).cloned();
    if let Some(a) = cached {
        let agent = &a;
        let mut applied = Vec::new();
        // Model goes through set_model, NOT a raw assignment.
        //
        // An alias is a NAME for a (provider, model, reasoning) triple.
        // Assigning it straight to model_override sent the alias name
        // itself to the provider as a model — which is how picking a
        // perfectly valid alias produced
        //   404 {"message":"Model not exist.","code":"model_not_found"}
        // set_model resolves aliases, `custom:<provider>` and bare model
        // names, and sets the provider (and any alias-pinned reasoning)
        // to match.
        if let Some(m) = &req.model {
            if !m.is_empty() {
                match agent.set_model(m) {
                    Ok(msg) => applied.push(msg),
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("{e:#}") })),
                        )
                    }
                }
            } else {
                agent.overrides.write_ok().model = None;
            }
        }
        // An explicit provider still wins over the one an alias implied.
        if let Some(p) = &req.provider {
            if p.is_empty() {
                agent.overrides.write_ok().provider = None;
            } else {
                agent.overrides.write_ok().provider = Some(p.clone());
                applied.push(format!("provider {p}"));
            }
        }
        if let Some(r) = &req.reasoning {
            if r.is_empty() {
                agent.overrides.write_ok().reasoning = None;
            } else {
                agent.overrides.write_ok().reasoning = Some(r.clone());
                applied.push(format!("reasoning {r}"));
            }
        }
        // The raw field assignments above bypassed set_model, so
        // persist the merged state once more — the row must match what
        // the live agent now says, or the next surface to open the
        // session resurrects what this one cleared.
        let _ = a.persist_overrides();
        return (
            StatusCode::OK,
            Json(json!({ "applied": "live", "detail": applied.join(" · ") })),
        );
    }
    (StatusCode::OK, Json(json!({ "applied": "deferred" })))
}

/// The agent for a session, built if this process has not touched it
/// yet — or a 404 that actually means "no such session".
///
/// THE BUG THIS RETIRES. Five handlers did
/// `srv.agents.lock_ok().get(&id).cloned()` and answered 404 "session
/// not active" on a miss. But the agents map is a CACHE, not a registry
/// of real sessions: a session that exists in the store and simply has
/// not run a turn in this process is a miss, and so is every session
/// after `reload_config()` — which empties the map on every
/// `config.toml` save, and there is a file watcher, so an editor save
/// is enough.
///
/// So `/plan on` typed into the desktop answered 404, the Tcl side
/// turned that into "plan mode unavailable (gateway offline?)" on a
/// gateway that was up, and the mode was never set. USER 2026-09-16:
/// *"I DID call /plan on that session"* — they did; nothing received
/// it. The same shape cost a steer once already (see
/// `a_steer_before_the_first_turn_is_not_lost`).
///
/// `agent_for` checks `session_exists` and builds on the live config,
/// so the only 404 left is the one that was always meant.
fn agent_or_404(
    srv: &Arc<DesktopServer>,
    id: &str,
) -> Result<Arc<Agent>, (StatusCode, Json<Value>)> {
    srv.agent_for(id).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })
}

// ── session orchestration: steer, branch, undo, retry ─────────────────

async fn session_failover(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mode = body["mode"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let a = match agent_or_404(&srv, &id) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let msg = match a.set_failover_mode(if mode.is_empty() { None } else { Some(&mode) }) {
        Ok(m) => m,
        Err(e) => format!("error: {e}"),
    };
    (StatusCode::OK, Json(json!({ "message": msg })))
}

/// `/plan` for the desktop surface (backport 2026-08-29 review). The
/// desktop's `/api/chat` runs turns DIRECTLY — it never passes the
/// gateway's builtin dispatcher — so the composer needs its own route;
/// same agent method the gateway builtin calls. Body: {"mode": "on"}.
///
/// The "mode" field also carries READ VERBS (`show`, `tasks`,
/// `status`, optionally + " <session-id>"), handled here through the
/// same renderer the `show_plan` tool and the shared `/plan` arm use —
/// `set_plan_mode` would answer them "unknown plan mode", and the
/// desktop's composer POSTs its whole argument here. The mode path is
/// tried only when the first word is not a verb, exactly like the
/// dispatcher's arm, so the two doors agree on what `/plan off` means.
async fn session_plan(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mode = body["mode"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_default();
    // `agent_for`, NOT a cache peek: plan mode is a decision about a
    // SESSION, and a session that has not run a turn in this process
    // (or any session at all after a config reload emptied the cache)
    // is exactly when someone reaches for it. See `agent_or_404`.
    let a = match agent_or_404(&srv, &id) {
        Ok(a) => a,
        Err(e) => return e,
    };
    // A desktop API caller is the operator: no principal, so the
    // cross-session read gate sees None and allows, matching every
    // other owner-surface command on this server.
    let arg = mode.trim();
    let msg = match arg.split_whitespace().next() {
        Some(word) if veles_core::plan::Verb::parse(word).is_some() => {
            let verb = veles_core::plan::Verb::parse(word).expect("just matched");
            let tail = arg[word.len()..].trim();
            let store = srv.store.lock_ok();
            match veles_core::plan::resolve_read_target(&store, tail, &id) {
                Ok(target) => veles_core::plan::render_state(&store, &target, &id, verb),
                Err(e) => format!("error: {e:#}"),
            }
        }
        _ => match a.set_plan_mode(if mode.is_empty() { None } else { Some(&mode) }) {
            Ok(m) => m,
            Err(e) => format!("error: {e}"),
        },
    };
    (StatusCode::OK, Json(json!({ "message": msg })))
}

/// `/body` for the desktop surface: session extra_body override.
/// Body: {"body": "<json-or-clear>"} (empty body reports/clears).
async fn session_extra_body(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let raw = body["body"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let a = match agent_or_404(&srv, &id) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let msg = match a.set_extra_body(if raw.is_empty() { None } else { Some(&raw) }) {
        Ok(m) => m,
        Err(e) => format!("error: {e}"),
    };
    (StatusCode::OK, Json(json!({ "message": msg })))
}

/// GET /api/sessions/:id/mechanism — the catalogue plus this session's
/// overrides.
///
/// This used to exist only as the empty-name branch of the POST handler,
/// while the route registered `post` alone — so the overlay's read
/// (overlays.tcl `_mechanisms_fill`) got `405 Method Not Allowed` and
/// the pane rendered "mechanisms unreachable" instead of a single row.
/// A GET cannot carry the `Json(body)` extractor the POST needs, so the
/// read half is its own handler and the route names both verbs, the same
/// shape `/api/sessions/:id/override` already uses.
async fn session_mechanism_view(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let agent = match srv.agent_for(&id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": e.to_string() })),
            )
        }
    };
    (StatusCode::OK, Json(mechanism_snapshot(&agent)))
}

/// The read payload, shared by the GET handler and the POST handler's
/// no-name branch so the two surfaces cannot drift.
fn mechanism_snapshot(a: &Agent) -> Value {
    let overrides: Vec<Value> = a
        .mechanism_overrides()
        .into_iter()
        .map(|(k, v)| json!({ "key": k, "value": v }))
        .collect();
    let all: Vec<Value> = veles_core::notes::catalogue()
        .iter()
        .map(|m| {
            json!({
                "key": m.kind.as_str(),
                "glyph": m.kind.glyph(),
                "config_key": m.config_key,
                "help": m.help,
                // The layer UNDER the session override. Without it the
                // manager cannot tell "off for this session" from "off
                // by default", and cannot offer to promote one to the
                // other.
                "silenced_by_default": a.mechanism_silenced_by_default(m.kind.as_str()),
            })
        })
        .collect();
    json!({ "mechanisms": all, "overrides": overrides })
}

/// POST /api/sessions/:id/mechanism — override what the engine is
/// allowed to do on its own for this session. An empty `name` reads
/// instead, which is how the `/mechanisms` slash command lists them.
///
/// Persisted in store `meta`, exactly like `/failover`'s per-session
/// mode, so `--resume` keeps it and there is one persistence mechanism
/// rather than two.
async fn session_mechanism(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let agent = match srv.agent_for(&id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": e.to_string() })),
            )
        }
    };
    let name = body["name"].as_str().unwrap_or("");
    if name.is_empty() {
        return (StatusCode::OK, Json(mechanism_snapshot(&agent)));
    }
    let value = body["value"].as_str();
    match agent.set_mechanism(name, value) {
        Ok(msg) => (StatusCode::OK, Json(json!({ "message": msg }))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

async fn session_steer(
    State(_srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let msg = body["message"].as_str().unwrap_or_default().to_string();
    if msg.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "message required" })),
        );
    }
    // ROUTED, not cached: `ask::steer` resolves through the
    // process-global steer slot the RUNNING turn published — whichever
    // agent cache built the agent (desktop's or the gateway's), so the
    // silent-loss "queued into an Option nobody reads" 200 is
    // structurally gone. No live turn → the steer DOCKS (process-wide)
    // and the next turn on this session start-injects it; the response
    // says which happened, because "parked" behind a compaction the
    // user believes is a turn is a hazard the client must be able to
    // name (PENDING 2026-09-14).
    match veles_core::ask::steer(&id, &msg) {
        veles_core::ask::Steered::Live => (
            StatusCode::OK,
            Json(json!({ "queued": msg, "state": "live" })),
        ),
        // A steer that lands in the same tool-round gap as another is
        // APPENDED, not substituted — it used to replace it and say
        // nothing. `behind` is how many were already waiting, so the
        // client can say so rather than implying this is the only one.
        veles_core::ask::Steered::LiveQueued { behind } => (
            StatusCode::OK,
            Json(json!({ "queued": msg, "state": "live", "behind": behind })),
        ),
        veles_core::ask::Steered::Parked => (
            StatusCode::OK,
            Json(json!({ "queued": msg, "state": "parked" })),
        ),
    }
}

/// `/clear` parity: drop the session's live context. The desktop loads
/// history from the store at the start of every turn, so archiving the
/// unarchived rows IS clearing the context (rows stay searchable, the
/// way `archive_head` always does — lineage is never deleted).
async fn session_clear(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    // Nothing left to retry once the context is cleared. The agent lock
    // is taken FIRST and released before the store is touched: a turn in
    // flight holds the agent and wants the store, so the inverse order
    // here is a textbook deadlock.
    let cached = srv.agents.lock_ok().get(&id).cloned();
    if let Some(a) = cached {
        a.forget_last_prompt();
    }
    let store = srv.store.lock_ok();
    // Every live row, not `archive_head(load_history().len())`: the
    // repaired load hides orphaned results, so that count left the
    // newest rows live (see `Store::archive_all_live`).
    match store.archive_all_live(&id) {
        Ok(n) => {
            // The transcript changed while NO turn was running — tell the
            // watchers to reload. `busy` cannot carry this (nothing was
            // live in the first place), and guessing from /context could
            // never fire it.
            let last = store.last_msg_id(&id).unwrap_or(0);
            veles_core::session_tap::history_changed(&id, "clear", last);
            (StatusCode::OK, Json(json!({ "archived": n })))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// `/compress` / `/compact` — SUMMARIZE the session's head now.
///
/// USER (2026-09-05): *"Yes /compact|/compress should have always ran
/// summarizer. Some previous session was being sleazy and not
/// implementing it properly."* This endpoint was just `archive_head` =
/// flip the archived flag on the oldest third — instant, lossy, no LLM
/// call, which is why it ran "way too fast" and confused everyone.
///
/// Now it drives the agent's real summarizer (`compact_forced`, the
/// same code the automatic path uses): choose the cut, one
/// non-streaming summarize call, archive the head rows, append the
/// `[CONTEXT COMPACTION]` marker. Needs the agent cached (a session
/// with no live agent has no model to summarize with).
///
/// ASYNC: the summarizer makes a real LLM call and can legitimately
/// take longer than the desktop's 10s POST timeout. Waiting for it
/// inline made `/compact` report "transport: timeout" while the
/// summarizer was STILL RUNNING and holding the agent lock — looking
/// for all the world like a hang. Now the endpoint returns "started"
/// immediately; a spawned task does the work (holding the agent lock
/// so a new turn serializes behind it, by design), records a
/// compaction note + the marker when it finishes, and publishes
/// `history-changed` so every attached client reloads the transcript
/// and its context segment the moment the rows land — no polling.
async fn session_compress(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    // Compression needs a real agent (the summarizer route rides it), so
    // BUILD one rather than refusing a session this process has not
    // happened to touch — "open it first" was advice about a cache the
    // caller cannot see. See `agent_or_404`.
    let a = match agent_or_404(&srv, &id) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let turn_lock = srv.turn_lock(&id);
    // Serialize with turns, honestly: the spawn below used to claim it
    // held "the agent lock so a new turn serializes behind it" but no
    // such lock existed any more — a turn that started during the
    // summarizer's await loaded a stale pre-prune head and its rows
    // could be re-archived by the compaction's cut, vanishing from
    // every later model context while still visible in history (the
    // "acts like I'm not there" prompt-level twin, PENDING
    // 2026-09-14). try_lock: a live turn means COMPETITION, and the
    // honest answer is "busy, retry", not a silent interleave.
    // `try_lock_owned` so the guard can move into the spawned task —
    // held across the summarizer's await, a chat POST on this session
    // waits its turn instead of interleaving.
    let Ok(guard_for_task) = turn_lock.try_lock_owned() else {
        return (
            StatusCode::CONFLICT,
            Json(
                json!({ "error": "a turn is running on this session — compress after it \
finishes (steers sent meanwhile are held and injected at the next turn)" }),
            ),
        );
    };
    let (store, session_id, before) = {
        // Read the pre-compact message count WITHOUT holding the agent
        // lock across the spawn (the count is cheap; the summarizer is
        // not). The store is Arc'd and cheap to clone.
        let agent = &a;
        let store = agent.store.clone();
        let sid = agent.session_id.clone();
        let before = store
            .lock_ok()
            .load_history(&sid)
            .map(|h| h.len())
            .unwrap_or(0);
        (store, sid, before)
    };
    // Do the actual compaction off the request path. The client gets
    // "started" now; the note/marker land when the summarizer returns.
    // The turn-lock guard (owned, from try_lock_owned above) moves INTO
    // the task and is held across the summarizer's await.
    veles_core::rt::spawn(async move {
        let _held = guard_for_task;
        let agent = &a;
        let mut history = match store.lock_ok().load_history(&session_id) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("compact {session_id}: load failed: {e:#}");
                return;
            }
        };
        let outcome = agent.compact_forced(&mut history).await;
        let failure = outcome
            .is_none()
            .then(|| agent.take_compaction_failure())
            .flatten();
        let after = store
            .lock_ok()
            .load_history(&session_id)
            .map(|h| h.len())
            .unwrap_or(before);
        // Record the TRUTH, not a formality. The old note always said
        // "compact done: N → N" even when compact_forced no-op'd — which
        // is why a /compact that did nothing looked identical to one
        // that worked.
        //
        // It then switched to `after < before` as the test, and that is
        // WRONG in the other direction now: tail pruning shrinks the
        // context without removing a single row (that is the point — the
        // tool rows stay paired with their tool_calls). So a compaction
        // that pruned 200 stale outputs but archived nothing would have
        // reported "no rows archived ... did not shrink the live
        // context" while doing exactly the right thing. The outcome's own
        // counters are the test.
        if let Ok(st) = store.lock() {
            let (summary, detail) = match &outcome {
                Some(o) if o.archived > 0 || o.pruned > 0 => (
                    o.report(before, after),
                    "Manual /compact. Older messages were summarized and archived, and stale \
                     tool outputs pruned to stubs. Nothing was deleted — session_search still \
                     finds the originals."
                        .to_string(),
                ),
                Some(_) => (
                    format!("compact ran, nothing to shrink ({before} → {after})"),
                    "The summarizer returned but the history was already at or under its \
                     target, so nothing was archived or pruned."
                        .to_string(),
                ),
                None => match &failure {
                    // A summary that failed is not "no cut point".
                    Some(why) => (
                        format!("compaction failed ({before} messages kept)"),
                        format!("{why}. The history is unchanged."),
                    ),
                    None => (
                        format!("compact did not run ({before} messages kept)"),
                        "No clean cut point in the history — nothing was summarized.".to_string(),
                    ),
                },
            };
            let _ = st.record_note(&session_id, "compaction", &summary, Some(&detail), None);
            // The note/marker changed the transcript OUTSIDE a turn, so
            // no turn event announces it. This is the ONE signal clients
            // use to reload — the old `_compact_poll` watched /context's
            // `busy` key and was structurally unable to see this finish,
            // because a compaction holds the agent lock yet is not a
            // turn (busy said false from the first tick).
            let last = st.last_msg_id(&session_id).unwrap_or(0);
            veles_core::session_tap::history_changed(&session_id, "compaction", last);
        }
    });
    (
        StatusCode::OK,
        Json(json!({ "started": true, "before": before })),
    )
}

/// The active profile + every known one. The Tcl `/profile` command
/// used to hardcode "default"; now it asks the engine.
async fn profile_view(State(_srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let active = veles_core::profiles::active_profile();
    let profiles: Vec<Value> = veles_core::profiles::list_profiles()
        .into_iter()
        .map(|(name, on)| json!({ "name": name, "active": on }))
        .collect();
    Json(json!({ "active": active, "profiles": profiles }))
}

async fn session_branch(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let store = srv.store.lock_ok();
    // The engine's /branch, exactly: a child session (the parent link
    // the sidebar draws the fork tree from) filled by the ONE copy
    // primitive. This handler used to mint its own id and re-append the
    // REPAIRED wire history row by row — no parent, and a copy that
    // differed from what /branch made of the same session.
    let new_id = crate::chat::new_session_id();
    let title = store
        .title(&id)
        .ok()
        .flatten()
        .map(|t| format!("{t} (branch)"));
    if let Err(e) = store.create_child_session(&new_id, &id, title.as_deref()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        );
    }
    let copied = match store.copy_history(&id, &new_id) {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    // A watcher attached to the new session (the desktop opens it right
    // after this returns) must reload rather than show a stale copy.
    let last = store.last_msg_id(&new_id).unwrap_or(0);
    veles_core::session_tap::history_changed(&new_id, "branch", last);
    (
        StatusCode::OK,
        Json(json!({
            "new_session_id": new_id,
            "messages_copied": copied
        })),
    )
}

async fn session_undo(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let n = body["n"].as_u64().unwrap_or(1) as u32;
    let store = srv.store.lock_ok();
    match store.undo_turns(&id, n) {
        Ok(dropped) => {
            let last = store.last_msg_id(&id).unwrap_or(0);
            veles_core::session_tap::history_changed(&id, "undo", last);
            (StatusCode::OK, Json(json!({ "dropped": dropped })))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

/// POST /api/sessions/:id/retry → 202 Accepted.
///
/// This endpoint used to REFUSE to start a turn — it handed the last
/// prompt back for the client to re-submit, its own comment explaining
/// why: "Spawning a turn directly here would need to wire up the SSE
/// event stream, which is session-scoped." The bus made that reason
/// obsolete: a turn is observable by anyone on
/// `GET /api/sessions/:id/events`, so retry is now the same
/// fire-and-forget spawn `chat()` does. `last_prompt` is remembered by
/// `run_turn` itself, so this retries the last turn no matter which
/// surface started it.
async fn session_retry(
    State(srv): State<Arc<DesktopServer>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let agent = match srv.agent_for(&id) {
        Ok(a) => a,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "session not active" })),
            )
        }
    };
    let Some(text) = agent.last_prompt() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no last prompt to retry" })),
        );
    };
    let sid = id.clone();
    let cancel_for_task = CancelToken::new();
    let turn_lock = srv.turn_lock(&sid);
    // The window's language (X-Veles-Locale) does not ride into a
    // spawned task on its own; carry it so the turn's engine text
    // follows the desktop (the session's own language still wins).
    let ui_locale = veles_core::i18n::scoped();
    veles_core::rt::spawn(async move {
        // Retry takes the SAME per-session turn lock POST /api/chat
        // serializes on. It never did — a retry racing a chat POST (or
        // a wake drain) interleaved two turns' history writes on one
        // session, exactly the overlap the asker bug needed to fire
        // (PENDING 2026-09-14 path 2). No asker install/clear either:
        // the desktop's asker is a session attach made in `agent_for`.
        let _turn = turn_lock.lock().await;
        let agent = &agent;
        let mut history = agent
            .store
            .lock()
            .unwrap()
            .load_history(&agent.session_id)
            .unwrap_or_default();
        let _ = veles_core::i18n::scope(
            ui_locale.clone(),
            agent.run_turn(&mut history, &text, Some(cancel_for_task), |_| {}),
        )
        .await;
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "session_id": id, "started": true })),
    )
}

/// GET /api/desktop/state · POST /api/desktop/state — desktop-owned JSON
/// blob (~/.local/share/veles/desktop_state.json): pins, theme, collapsed sidebar.
/// The backend is just durable storage; the UI owns the schema.
async fn get_desktop_state(State(_srv): State<Arc<DesktopServer>>) -> Json<Value> {
    Json(load_desktop_state())
}

async fn put_desktop_state(
    State(_srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    match save_desktop_state(&body) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

fn state_path() -> std::path::PathBuf {
    veles_core::config::veles_home().join("desktop_state.json")
}

fn load_desktop_state() -> Value {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_desktop_state(v: &Value) -> anyhow::Result<()> {
    std::fs::write(state_path(), serde_json::to_string_pretty(v)?)?;
    Ok(())
}

/// Pub re-exports for the panes backend (preview/focus state rides the
/// same durable desktop_state blob).
pub fn load_desktop_state_pub() -> Value {
    load_desktop_state()
}
pub fn save_desktop_state_pub(v: &Value) -> anyhow::Result<()> {
    save_desktop_state(v)
}

// ── setup wizard (PLAN-setup-wizard-tk.md) ───────────────────────────
//
// The Tk wizard's whole backend. Three routes, all behind the same auth
// gate as every other mutation endpoint (bearer token, or loopback-only
// in open mode):
//
//   GET  /api/setup/state    is this a first run? what does the file say?
//   POST /api/setup/preview  the exact bytes a write would produce
//   POST /api/setup/apply    write (edit in place), reload the live config
//
// All three are thin JSON seams over `veles_core::setup` — the ONE
// implementation `veles setup` also drives, so the two wizards cannot
// emit different TOML (the `60786dd` drift this ends).
//
// SECRETS: the answers carry env-var NAMES (`key_env`; the bot token
// and the SASL password are implied by their steps) — `setup_plan`
// refuses a `key_env` that is not a variable name, without echoing it.
// VALUES travel in exactly one place: `apply`'s `secrets` object, which
// is refused from any peer but loopback or a `[vault] trusted_networks`
// network, checked against the names the
// plan implies, and stored straight into the vault. `preview` never
// takes one, and `state` and `preview` say only which names are
// already stored. Nothing here logs a body; the `tracing` lines name the
// route and the OUTCOME only, and responses carry names, never values.

/// GET /api/setup/state — what the wizard needs before its first screen:
/// is this a first run (no config.toml at all), and — when there is a
/// file — the answers it already gives (`current`), so a re-run starts
/// from what is there and a walk that changes nothing writes nothing.
///
/// `first_run` is ONLY "config.toml is absent" — an operator with a
/// hand-rolled config never sees the wizard fire, whatever else is
/// missing in it. The "don't ask again" marker is the desktop's own
/// (desktop_state.json `setup_done`), deliberately NOT config.toml: a
/// config copied between machines must not carry machine-local markers
/// (the node.toml lesson).
///
/// `current` is read from the FILE, which is what an edit will start
/// from — not the live handle, which can be a boot snapshot of a file
/// that has changed since.
/// The wizard's secret names that already resolve somewhere (the
/// vault, the environment, the env files), so a re-run shows "stored —
/// leave blank to keep it" instead of an empty field that reads as
/// missing.
fn setup_stored(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = names
        .into_iter()
        .filter(|n| !n.is_empty() && veles_core::secrets::resolve(n).is_some())
        .collect();
    v.sort();
    v.dedup();
    v
}

async fn setup_state(
    State(_srv): State<Arc<DesktopServer>>,
    peer: Option<ConnectInfo<SocketAddr>>,
) -> Json<Value> {
    let dir = veles_core::config::config_dir();
    let cfg_path = dir.join("config.toml");
    let env_path = dir.join(".env");
    let (current, current_error) = match veles_core::setup::read_existing(&cfg_path) {
        Ok(None) => (Value::Null, Value::Null),
        Ok(Some(text)) => match veles_core::setup::current_answers(&text) {
            Ok(a) => (serde_json::to_value(a).unwrap_or(Value::Null), Value::Null),
            // The wizard's job is partly to help with a broken file, so
            // this is said, not a 500.
            Err(e) => (Value::Null, json!(format!("{e:#}"))),
        },
        Err(e) => (Value::Null, json!(format!("{e:#}"))),
    };
    let key_env = current["key_env"].as_str().unwrap_or("").to_string();
    // Every preset's key name too: the provider step says "stored"
    // for whichever provider the operator picks, not only the file's.
    let stored = setup_stored(
        [
            key_env,
            "TELEGRAM_BOT_TOKEN".to_string(),
            veles_core::setup::IRC_SASL_PASSWORD.to_string(),
        ]
        .into_iter()
        .chain(
            veles_core::setup::PROVIDER_PRESETS
                .iter()
                .map(|p| p.key_env.to_string()),
        ),
    );
    Json(json!({
        "first_run": !cfg_path.exists(),
        "config_exists": cfg_path.exists(),
        "config_path": cfg_path.display().to_string(),
        "env_path": env_path.display().to_string(),
        "env_exists": env_path.exists(),
        "current": current,
        "current_error": current_error,
        "stored": stored,
        // May this caller send secret VALUES (see `secrets_set`)? A
        // desktop that may not is told where to enter them instead.
        "accepts_secrets": accepts_values(&peer),
        "vault_path": veles_core::vault::vault_dir().display().to_string(),
        // The provider step renders these rows (veles_core::setup).
        "presets": veles_core::setup::PROVIDER_PRESETS,
    }))
}

#[derive(Default, serde::Deserialize)]
pub(crate) struct SetupModelsBody {
    base_url: String,
    /// The key's NAME: a stored value is used when `key` is empty.
    #[serde(default)]
    key_env: String,
    /// A value typed on the provider step and not yet stored.
    #[serde(default)]
    key: String,
}

/// The model step's list: the provider's own `/models`, fetched with the
/// key the operator just typed (same trust rule as `apply`) or the one
/// already stored under `key_env`. Never stores anything; a failure is a
/// 200 with `error`, because the step then falls back to a typed name.
async fn setup_models(
    peer: Option<ConnectInfo<SocketAddr>>,
    Json(body): Json<SetupModelsBody>,
) -> (StatusCode, Json<Value>) {
    if !body.key.trim().is_empty() && !accepts_values(&peer) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": values_refused() })),
        );
    }
    let key = if body.key.trim().is_empty() {
        veles_core::secrets::resolve(body.key_env.trim()).unwrap_or_default()
    } else {
        body.key.trim().to_string()
    };
    match veles_core::setup::list_models(&body.base_url, &key).await {
        Ok(models) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "models": models })),
        ),
        Err(e) => (
            StatusCode::OK,
            Json(json!({ "ok": false, "models": [], "error": format!("{e:#}") })),
        ),
    }
}

/// Optional component fields are flattened so an ordinary setup body
/// keeps its original wire shape and all existing callers stay valid.
#[derive(Default, serde::Deserialize)]
pub(crate) struct SetupPreviewBody {
    #[serde(flatten)]
    answers: veles_core::setup::SetupAnswers,
    #[serde(default)]
    component_name: Option<String>,
    #[serde(default)]
    instance: String,
    #[serde(default)]
    options: std::collections::BTreeMap<String, Value>,
}

impl SetupPreviewBody {
    fn component(&self) -> anyhow::Result<Option<veles_install::ComponentRequest>> {
        match self.component_name.as_deref() {
            None | Some("") => {
                anyhow::ensure!(
                    self.options.is_empty() && self.instance.is_empty(),
                    "component options require a selected component"
                );
                Ok(None)
            }
            Some(name) => Ok(Some(veles_install::ComponentRequest {
                name: name.to_string(),
                instance: self.instance.clone(),
                values: self.options.clone(),
            })),
        }
    }
}

/// POST /api/setup/preview — the would-be config.toml, verbatim, and the
/// signature of the file it was computed from.
///
/// The ACL overlay's preview-gates-"Write it" is the pattern: the wizard
/// shows this on its summary screen and sends `base_sig` back with
/// `apply`, which refuses when the file changed in between — so the
/// bytes the operator read are the bytes that land. Pure — writes nothing.
pub(crate) async fn setup_preview(Json(body): Json<SetupPreviewBody>) -> (StatusCode, Json<Value>) {
    let path = veles_core::config::config_dir().join("config.toml");
    let component = match body.component() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"error":format!("{e:#}")})),
            )
        }
    };
    let answers = body.answers;
    if component.is_some() {
        let dirs = veles_core::config::dirs();
        return crate::desktop_install::setup_combined_preview(dirs, answers, component).await;
    }
    let existing = match veles_core::setup::read_existing(&path) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": format!("{e:#}") })),
            )
        }
    };
    match veles_core::setup::setup_plan(&answers, existing.as_deref()) {
        Ok(plan) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "mode": plan.mode,
                "base_sig": plan.base_sig,
                // Over an existing file, the lines that change — what the
                // summary shows first; the whole text is still here.
                "diff": veles_core::setup::preview_diff(
                    existing.as_deref().unwrap_or(""),
                    &plan.config_toml,
                ),
                "config_toml": plan.config_toml,
                "env_text": plan.env_text,
                // NAMES only (see the tripwire comment above the module).
                "stored": setup_stored(plan.secrets.iter().cloned()),
                "secrets": plan.secrets,
                "notes": plan.notes,
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": format!("{e:#}") })),
        ),
    }
}

/// The apply body: the same answers `preview` took, plus the `base_sig`
/// that preview returned.
#[derive(serde::Deserialize)]
pub(crate) struct SetupApplyBody {
    #[serde(flatten)]
    answers: veles_core::setup::SetupAnswers,
    #[serde(default)]
    expect_base: Option<String>,
    /// Secret VALUES by name (the provider key, the bot token, the SASL
    /// password) — the one field in this API that carries one. Empty
    /// values mean "keep what is stored".
    #[serde(default)]
    secrets: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    component_name: Option<String>,
    #[serde(default)]
    instance: String,
    #[serde(default)]
    options: std::collections::BTreeMap<String, Value>,
    #[serde(default)]
    plan_sig: Option<String>,
    #[serde(default)]
    confirm: bool,
}

// ── secrets: names for anyone authorised, values from THIS machine ──
//
// `/api/secrets/set` is the one route that carries a secret value: the
// desktop's masked field (the `secret_request` dialog and `/secret set`)
// and nothing else. The desktop API is plain HTTP, so a value is taken
// only from a caller whose path here is not the open network: loopback,
// or a network the operator listed in `[vault] trusted_networks` — a
// WireGuard subnet, the laptop's desktop driving the phone's gateway
// (USER, 2026-09-27: "a lot of this stuff I'll be doing over
// wireguard"). The value is never echoed, logged, or put in an error.

#[derive(Deserialize)]
struct SecretSetBody {
    name: String,
    value: String,
    /// The `secret_request` ask this answers. Its NAME must match, and
    /// the hosts the model asked for come from the server's own record
    /// of the request, never from this body.
    #[serde(default)]
    ask_id: Option<String>,
}

/// May this caller send secret VALUES (`secrets::accepts_secret_values`,
/// read against the LIVE config so a newly listed network needs no
/// restart)? No ConnectInfo means no listener — a direct call in a test
/// — which `require_desktop_auth` treats as local too.
fn accepts_values(peer: &Option<ConnectInfo<SocketAddr>>) -> bool {
    match peer {
        None => true,
        Some(ConnectInfo(a)) => {
            veles_core::secrets::accepts_secret_values(a.ip(), &veles_core::config::live())
        }
    }
}

/// The refusal, in the operator's language: where to type it instead.
fn values_refused() -> String {
    veles_core::i18n::lookup(
        "secret values are accepted only from the gateway's own machine or a network in its [vault] trusted_networks — enter them there with `veles secret set`",
        &[],
    )
}

async fn secrets_list(peer: Option<ConnectInfo<SocketAddr>>) -> Json<Value> {
    let rows: Vec<Value> = veles_core::secrets::listing()
        .into_iter()
        .map(|l| {
            json!({
                "name": l.name,
                "kind": l.policy.kind.as_str(),
                "shell": l.policy.shell,
                "hosts": l.policy.hosts,
                "note": l.policy.note,
                "source": l.source,
            })
        })
        .collect();
    // Whether THIS caller may send values: the desktop asks before it
    // offers a masked field, so nobody types a key for nothing.
    Json(json!({ "secrets": rows, "accepts_values": accepts_values(&peer) }))
}

async fn secrets_set(
    peer: Option<ConnectInfo<SocketAddr>>,
    Json(body): Json<SecretSetBody>,
) -> (StatusCode, Json<Value>) {
    let refuse = |code: StatusCode, e: String| (code, Json(json!({ "ok": false, "error": e })));
    let name = body.name.trim().trim_start_matches('$').to_string();
    if !accepts_values(&peer) {
        return refuse(StatusCode::FORBIDDEN, values_refused());
    }
    let policy = match body.ask_id.as_deref().filter(|s| !s.is_empty()) {
        Some(id) => match veles_core::secret_tools::pending(id) {
            Some(p) if p.name == name => veles_core::secret_tools::policy_for_request(&p),
            Some(p) => {
                return refuse(
                    StatusCode::BAD_REQUEST,
                    format!("that request was for {}, not {name}", p.name),
                )
            }
            None => {
                return refuse(
                    StatusCode::CONFLICT,
                    "that request is no longer open".to_string(),
                )
            }
        },
        None => veles_core::secrets::policy_of(&name, &veles_core::config::live()),
    };
    let value = body.value;
    let n = name.clone();
    let stored =
        veles_core::rt::spawn_blocking(move || veles_core::vault::set_local(&n, &value, policy))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .and_then(|r| r);
    match stored {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "name": name, "stored": "vault" })),
        ),
        Err(e) => refuse(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// POST /api/setup/apply — write, and pull the result into the live
/// config handle.
///
/// The plan is rebuilt here from the answers and the file as it is NOW,
/// rather than trusted from the client; `expect_base` then says whether
/// "now" is still the file the operator previewed. If it is not (a hand
/// edit, `/model`, another desktop), the answer is 409 and nothing is
/// written — the preview has to be looked at again.
async fn setup_apply(
    State(srv): State<Arc<DesktopServer>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Json(body): Json<SetupApplyBody>,
) -> (StatusCode, Json<Value>) {
    let refuse = |code: StatusCode, e: String| (code, Json(json!({ "ok": false, "error": e })));
    // Before anything is read or written: a secret value from an
    // untrusted network has already crossed it in the clear.
    if body.secrets.values().any(|v| !v.trim().is_empty()) && !accepts_values(&peer) {
        return refuse(StatusCode::FORBIDDEN, values_refused());
    }
    let dir = veles_core::config::config_dir();
    let component = match (
        body.component_name.as_deref(),
        body.options.is_empty(),
        body.instance.is_empty(),
    ) {
        (None | Some(""), true, true) => None,
        (Some(name), _, _) => Some(veles_install::ComponentRequest {
            name: name.to_string(),
            instance: body.instance.clone(),
            values: body.options.clone(),
        }),
        _ => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "component options require a selected component".into(),
            )
        }
    };
    if component.is_some() {
        let dirs = veles_core::config::dirs();
        let applied = crate::desktop_install::setup_combined_apply(
            dirs,
            srv.clone(),
            crate::desktop_install::CombinedApply {
                answers: body.answers,
                component,
                expect_base: body.expect_base,
                plan_sig: body.plan_sig,
                secrets: body.secrets,
                confirm: body.confirm,
            },
        )
        .await;
        if applied.0 == StatusCode::OK {
            tracing::info!("setup apply: combined setup/component configuration written");
        }
        return applied;
    }
    let existing = match veles_core::setup::read_existing(&dir.join("config.toml")) {
        Ok(t) => t,
        Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let plan = match veles_core::setup::setup_plan(&body.answers, existing.as_deref()) {
        Ok(p) => p,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    if body
        .expect_base
        .as_deref()
        .is_some_and(|b| b != plan.base_sig)
    {
        return refuse(
            StatusCode::CONFLICT,
            veles_core::setup::StaleBase.to_string(),
        );
    }
    // A name the answers do not use is the caller's mistake (400), and
    // it is caught before anything is written.
    if let Err(e) = veles_core::setup::check_secret_names(&plan, &body.secrets) {
        return refuse(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let secrets = body.secrets;
    let applied = veles_core::rt::spawn_blocking(move || {
        veles_core::setup::apply_with(&plan, &dir, &secrets).map(|r| (r, plan))
    })
    .await;
    let (report, plan) = match applied {
        Ok(Ok(r)) => r,
        Ok(Err(e)) if e.downcast_ref::<veles_core::setup::StaleBase>().is_some() => {
            return refuse(StatusCode::CONFLICT, e.to_string())
        }
        Ok(Err(e)) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    // Same discipline as `config_set` (R4.5): the file landed, so the
    // live handle must see it NOW — serving the boot snapshot after a
    // successful write is how a desktop ends up saying one thing while
    // the config says another. Agents are evicted too (reload_config
    // clears the cache), so the next session routes through the new
    // provider rather than a cached agent built from the old one.
    let reloaded = match srv.reload_config() {
        Ok(_) => true,
        Err(e) => {
            // The write SUCCEEDED; only the hot-reload failed (a broken
            // neighbouring file the wizard did not write). Say both.
            tracing::warn!("setup apply: config written but live reload failed: {e:#}");
            false
        }
    };
    tracing::info!(
        "setup apply: wrote {} ({:?}, backup: {}, .env written: {}, live reload: {})",
        report.config_path.display(),
        report.mode,
        report.config_backup.is_some(),
        report.env_written,
        reloaded,
    );
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "mode": report.mode,
            "config_path": report.config_path.display().to_string(),
            "config_backup": report.config_backup.map(|p| p.display().to_string()),
            "env_path": report.env_path.display().to_string(),
            "env_written": report.env_written,
            "env_added": report.env_added,
            "secrets_needed": report.secrets_needed,
            "secrets_stored": report.secrets_stored,
            "vault_path": veles_core::vault::vault_dir().display().to_string(),
            "notes": plan.notes,
            "reloaded": reloaded,
        })),
    )
}

/// Desktop API auth gate. Every route except `/api/health` requires a
/// valid bearer — and since a token is now MINTED when none is configured
/// (`resolve_auth`), that is the posture a default install comes up in.
/// The open branch below is what `VELES_DESKTOP_OPEN=1` buys, plus the
/// fallback when the config directory cannot be written: loopback peers
/// only, and a bind that refuses to widen (`authn::resolve_bind_host`).
///
/// The config arrives as layer STATE — an `AuthConfig` resolved once in
/// `serve` — not by reading the environment here. Read `veles_core::authn`'s
/// docstring for why: the shape this replaced re-read `$VELES_HOME/.env`
/// on every request, so truncating or rotating that file while the server
/// ran flipped it back to unauthenticated, on a bind that had been widened
/// to the network precisely because a token existed. Passing the snapshot
/// in also means a test can build any posture it likes without touching
/// the environment.
///
/// `/api/health` stays open because the CLI (`veles-cli`), the backup
/// probe and `scripts/vps-test.sh` all use it as a liveness check and
/// none of them carries a token.
///
/// In TOKEN mode the client IP is an identity label only, never a trust
/// gate — a mobile client roams, and an IP allow/deny would drop it at
/// every handover.
///
/// In OPEN mode the peer must be loopback, which makes the code say what
/// the docstring always claimed. By construction it already is —
/// `authn::resolve_bind_host` refuses to widen the bind without a token —
/// so this catches only what the bind cannot see: a reverse proxy, socket
/// activation, or a port forward putting the server somewhere it never
/// agreed to be.
///
/// The peer address is read from the request extensions (populated by
/// `into_make_service_with_connect_info` at serve time) rather than as an
/// extractor parameter, so the gate is unit-testable without a live socket.
async fn require_desktop_auth(
    State(cfg): State<Arc<veles_core::authn::AuthConfig>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    // A challenge relay page carries its own credential: a random token
    // in its path, alive only while its relay runs (Part E). It is opened
    // from a chat client, which has no bearer token to send.
    // A module's OPEN route carries a credential of its own too (the
    // module checks it): the slopdrop bridge's Bluesky reads.
    if req.uri().path() == "/api/health"
        || req.uri().path().starts_with("/challenge/")
        || is_module_open_route(req.uri().path())
    {
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response()
    };
    if cfg.is_open() {
        // No ConnectInfo means no listener — a direct `oneshot` in a test.
        // Treat that as local, the same way an empty client IP is labelled
        // "local" by `authenticate`.
        if let Some(ip) = peer {
            if !ip.is_loopback() {
                tracing::warn!("desktop API: refusing {ip} — open mode is loopback-only");
                return unauthorized();
            }
        }
        return next.run(req).await;
    }
    let ip = peer.map(|i| i.to_string()).unwrap_or_default();
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if cfg.authenticate(auth.as_deref(), &ip).is_none() {
        return unauthorized();
    }
    next.run(req).await
}

/// The whole route table.
///
/// Split out of `serve` so a test can BUILD it. axum's router panics on
/// a route conflict at construction time, which — while the table lived
/// inside `serve` — meant the only way to find out was starting the
/// gateway. `router_builds` in the tests below is that check now.
pub fn router(srv: Arc<DesktopServer>, auth: Arc<veles_core::authn::AuthConfig>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/challenge/:token", get(challenge_page))
        .route("/challenge/:token/shot", get(challenge_shot))
        .route("/challenge/:token/state", get(challenge_state))
        .route("/challenge/:token/answer", post(challenge_answer))
        .route("/api/status", get(status))
        .route("/api/guards", get(guards))
        .route("/api/sessions/:id/context", get(context_view))
        .route("/api/config/failover", post(save_failover))
        .route(
            "/api/failover/script",
            get(failover_script).post(save_failover_script),
        )
        .route("/api/failover/scripts", get(list_failover_scripts))
        // Live-interpreter surfaces (loopback only): REPL / debugger /
        // hot-patch eval + manual plugin reload.
        .route("/api/tcl/eval", post(tcl_eval))
        .route("/api/tcl/reload", post(tcl_reload))
        // The reverse channel into the desktop window's own interpreter.
        .route("/api/desktop/eval", post(desktop_eval))
        .route("/api/desktop/eval/events", get(desktop_eval_events))
        .route("/api/desktop/eval/result", post(desktop_eval_result))
        .route("/api/desktop/eval/arm", post(desktop_eval_arm))
        .route("/api/desktop/eval/status", get(desktop_eval_status))
        // The device bridge (veles_core::devices): a window announces
        // what it can do, listens for calls, answers them; the
        // `device` tool lists and calls through these.
        .route("/api/device/events", get(device_events))
        .route("/api/device/hello", post(device_hello))
        .route("/api/device/result", post(device_result))
        .route("/api/device/call", post(device_call))
        .route("/api/device/event", post(device_event))
        .route("/api/device/list", get(device_list))
        .route("/api/config", get(config_view))
        .route("/api/config/get", get(config_get))
        .route("/api/config/form", get(config_form))
        .route("/api/pool/scan", post(pool_scan))
        .route("/api/apps", get(apps_list))
        .route("/api/apps/:name/source", get(app_source))
        .route("/api/apps/:name/store", post(app_store))
        .route("/api/apps/:name/ask", post(app_ask))
        .route("/api/pool/join", post(pool_join))
        .route("/api/config/set", post(config_set))
        .route("/api/config/reload", post(config_reload))
        .route("/api/search", get(search))
        .route("/api/skills", get(skills_list))
        .route("/api/skills/new", post(skill_new))
        .route("/api/skills/install", post(skill_install))
        .route(
            "/api/skills/:name",
            get(skill_view).post(skill_save).delete(skill_delete),
        )
        .route("/api/memory", get(memory_view).post(memory_write))
        .route("/api/memory/log", get(memory_log))
        .route("/api/memory/rollback", post(memory_rollback))
        .route("/api/memory/files", get(memory_files))
        .route(
            "/api/memory/file",
            get(memory_file_get).post(memory_file_put),
        )
        .route("/api/memory/file/delete", post(memory_file_delete))
        .route("/api/memory/file/move", post(memory_file_move))
        .route("/api/memory/restore", post(memory_restore))
        .route("/api/memory/diff", get(memory_diff))
        .route("/api/hooks", get(hooks_list))
        .route("/api/hooks/toggle", post(hooks_toggle))
        .route("/api/plugins", get(plugins_list))
        .route("/api/plugins/new", post(plugin_new))
        .route("/api/plugins/install", post(plugin_install))
        .route("/api/plugins/history", get(plugin_history))
        .route("/api/plugins/rollback", post(plugin_rollback))
        .route("/api/plugins/:name", delete(plugin_delete))
        .route(
            "/api/plugins/:name/:action",
            post(plugin_set_enabled).get(module_route_get),
        )
        .route(
            "/api/plugins/:name/open/:route",
            get(module_open_route).post(module_open_route),
        )
        .route("/api/process-plugins", get(process_plugins_status))
        .route(
            "/api/process-plugins/:target/logs",
            get(process_plugin_logs),
        )
        .route(
            "/api/process-plugins/:target/:action",
            post(process_plugin_control),
        )
        // R3 (M6): the generic plugin runtime surface. `/api/plugins/runtime`
        // lists BOTH tiers (process + module); `/api/plugins/runtime/rescan`
        // re-discovers under both hosts' roots (a first install needs no
        // restart); the per-target verbs act on whichever tier hosts the
        // target. The old `/api/process-plugins*` names stay as aliases.
        .route("/api/plugins/runtime", get(plugins_runtime_status))
        .route("/api/plugins/services", get(plugins_services))
        .route("/api/plugins/overrides", get(plugins_overrides))
        .route("/api/plugins/runtime/rescan", post(plugins_runtime_rescan))
        .route(
            "/api/plugins/runtime/:target/:action",
            post(plugins_runtime_control),
        )
        .route("/api/cron", get(cron_list))
        .route("/api/cron/new", post(cron_new))
        .route("/api/cron/describe", get(cron_describe))
        .route("/api/cron/:id", post(cron_edit).delete(cron_delete))
        .route("/api/cron/:id/history", get(cron_history))
        .route("/api/cron/:id/pause", post(cron_pause))
        .route("/api/cron/:id/resume", post(cron_resume))
        .route("/api/cron/:id/run", post(cron_run))
        .route("/api/triggers", get(triggers_list))
        .route("/api/triggers/new", post(trigger_new))
        .route(
            "/api/triggers/:name",
            post(trigger_edit).delete(trigger_delete),
        )
        .route("/api/triggers/:name/history", get(trigger_history))
        .route("/api/triggers/:name/:action", post(trigger_set_enabled))
        .route("/api/slopdrop", get(slopdrop_status_alias))
        .route("/api/slopdrop/sync", post(slopdrop_sync_alias))
        .route("/api/slopdrop/save", post(slopdrop_save_alias))
        .route("/api/usage", get(usage))
        .route("/api/quota", get(quota))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/new", post(new_session))
        .route("/api/sessions/delete", post(delete_sessions))
        .route("/api/sessions/:id", delete(delete_session))
        .route("/api/sessions/:id/head", get(session_head))
        .route("/api/sessions/:id/history", get(history))
        // Attach to a session's live event stream. The counterpart to
        // history: that returns the transcript, this returns the
        // transcript AND everything after it, including a turn already
        // in flight. See `session_events`.
        .route("/api/sessions/:id/events", get(session_events))
        // Events from a turn in ANOTHER process — see `relay_ingest`.
        .route(veles_core::relay::RELAY_PATH, post(relay_ingest))
        .route("/api/sessions/:id/title", post(set_title))
        .route(
            "/api/sessions/:id/queue",
            get(queue_view)
                .post(queue_push)
                .patch(queue_patch)
                .delete(queue_delete),
        )
        .route(
            "/api/sessions/:id/override",
            post(set_override).get(get_override),
        )
        .route("/api/sessions/:id/steer", post(session_steer))
        .route("/api/sessions/:id/cancel", post(session_cancel))
        .route("/api/sessions/:id/detach", post(session_detach))
        .route("/api/sessions/:id/clear", post(session_clear))
        .route("/api/sessions/:id/compress", post(session_compress))
        .route("/api/profile", get(profile_view))
        .route("/api/sessions/:id/failover", post(session_failover))
        .route("/api/sessions/:id/plan", post(session_plan))
        .route("/api/sessions/:id/body", post(session_extra_body))
        .route(
            "/api/sessions/:id/mechanism",
            post(session_mechanism).get(session_mechanism_view),
        )
        .route("/api/sessions/:id/skill", post(session_skill))
        .route("/api/sessions/:id/branch", post(session_branch))
        .route("/api/sessions/:id/undo", post(session_undo))
        .route("/api/sessions/:id/retry", post(session_retry))
        .route("/api/logs", get(logs))
        .route("/api/themes", get(themes))
        .route("/api/tools", get(list_tools))
        .route("/api/join", post(crate::desktop_join::join_request))
        .route("/api/join/status", get(crate::desktop_join::status_request))
        .route("/api/join/leave", post(crate::desktop_join::leave_request))
        .route("/api/commands", get(commands_list))
        .route("/api/commands/render", post(commands_render))
        .route("/api/btw", post(btw_start))
        .route("/api/shells", post(shell_start))
        .route("/api/tasks", get(tasks_list))
        .route("/api/tasks/:id/output", get(task_output))
        .route("/api/tasks/:id/cancel", post(task_cancel))
        .route("/api/agents", get(agents_list))
        .route("/api/asks", get(list_asks))
        .route("/api/sessions/:id/todos", get(session_todos))
        .route("/api/checkpoints", get(list_checkpoints))
        .route("/api/checkpoints/:id/restore", post(restore_checkpoint))
        .route("/api/approvals/:id/decide", post(decide_ask))
        .route(
            "/api/desktop/state",
            get(get_desktop_state).post(put_desktop_state),
        )
        // Setup wizard (PLAN-setup-wizard-tk.md): preview-gates-apply,
        // the ACL promote pattern. All three behind require_desktop_auth.
        .route("/api/setup/state", get(setup_state))
        .route("/api/setup/preview", post(setup_preview))
        .route("/api/setup/apply", post(setup_apply))
        .route("/api/setup/models", post(setup_models))
        // R11 typed component installation. Every route is behind the
        // same bearer/loopback auth layer as setup. A peer never chooses
        // a config output path or sends replacement config text.
        .route("/api/install/state", get(crate::desktop_install::state))
        .route(
            "/api/install/components",
            get(crate::desktop_install::components),
        )
        .route(
            "/api/install/options/:name",
            get(crate::desktop_install::options),
        )
        .route(
            "/api/install/preview",
            post(crate::desktop_install::preview),
        )
        .route("/api/install/apply", post(crate::desktop_install::apply))
        .route(
            "/api/install/profile",
            post(crate::desktop_install::profile),
        )
        .route("/api/install/bundle", post(crate::desktop_install::bundle))
        .route(
            "/api/install/download",
            post(crate::desktop_install::signed_download),
        )
        .route(
            "/api/install/download/confirm",
            post(crate::desktop_install::signed_download_confirm),
        )
        .route(
            "/api/install/component",
            post(crate::desktop_install::component),
        )
        .route(
            "/api/install/rollback",
            post(crate::desktop_install::rollback),
        )
        .route("/api/install/verify", post(crate::desktop_install::verify))
        .route("/api/secrets", get(secrets_list))
        .route("/api/secrets/set", post(secrets_set))
        // Right-pane shell: Files / Review / Preview (+ focus), hermes-desktop
        // right-sidebar parity. The agent reaches these via open_preview /
        // focus_pane tools over the same loopback API.
        .route("/api/fs/tree", get(panes::fs_tree))
        .route("/api/fs/read", get(panes::fs_read))
        // The Dev pane editor's whole-file read. fs_read is a preview
        // HEAD (line-limited); opening a buffer through it truncated
        // every file past the limit and fed the truncation to the LSP.
        .route("/api/fs/file", get(panes::fs_file))
        .route("/api/fs/write", post(panes::fs_write))
        .route("/api/git/status", get(panes::git_status))
        .route("/api/git/diff", get(panes::git_diff))
        .route("/api/git/mutate", post(panes::git_mutate))
        .route(
            "/api/preview",
            get(panes::preview_get).post(panes::preview_set),
        )
        .route("/api/focus", get(panes::focus_get).post(panes::focus_set))
        .route("/api/irc/state", get(irc_state))
        .route("/api/irc/backlog", get(irc_backlog))
        .route("/api/irc/events", get(irc_events))
        .route("/api/notify/events", get(notify_events))
        .route("/api/irc/send", post(irc_send))
        .route("/api/irc/typing", post(irc_typing))
        .route("/api/irc/op", post(irc_op))
        .route("/api/acl", get(acl_view))
        .route("/api/acl/explain", get(acl_explain))
        .route("/api/acl/limits", post(acl_limit_set))
        .route("/api/lockdown", post(lockdown_set))
        .route("/api/defaults", post(defaults_run))
        .route("/api/acl/grant", post(acl_grant))
        .route("/api/acl/revoke", post(acl_revoke))
        .route("/api/acl/user", post(acl_user_save))
        .route("/api/acl/user/delete", post(acl_user_delete))
        .route("/api/acl/promote", post(acl_promote))
        .route("/api/sandboxes", get(sandboxes_view))
        .route("/api/sandboxes/create", post(sandbox_create))
        .route("/api/sandboxes/drop", post(sandbox_drop))
        .route("/api/sandboxes/purge", post(sandbox_purge))
        .route("/api/sandboxes/assign", post(sandbox_assign))
        .route("/api/sandboxes/unassign", post(sandbox_unassign))
        // The engine's SLASH-command table, so the desktop stops
        // keeping three hand-maintained copies of it. `/api/commands`
        // is taken, and by a different real thing — the custom
        // commands from `.veles/commands/` and the plugins.
        .route("/api/commands/builtin", get(commands_view))
        // Run one SHARED command for a session — the seam that stops
        // the desktop reimplementing the engine's command set in Tcl.
        .route("/api/command", post(command_run))
        .route("/api/lsp/start", post(lsp_start))
        .route("/api/lsp/send", post(lsp_send))
        .route("/api/lsp/events", get(lsp_events))
        .route("/api/lsp/stop", post(lsp_stop))
        .route("/api/lsp/status", get(lsp_status))
        .route("/api/chat", post(chat))
        // Axum's default request-body cap is 2MB, and the Dev pane now
        // ships whole files through two POSTs: didOpen/didChange to
        // /api/lsp/send and saves to /api/fs/write. The editor side is
        // capped at 8MB (fs_file), and JSON escaping can grow a body —
        // 16MB leaves headroom for the worst case without opening the
        // loopback API to arbitrary payloads.
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(middleware::from_fn(locale_scope))
        .layer(middleware::from_fn(idle_touch))
        .layer(middleware::from_fn_with_state(
            auth.clone(),
            require_desktop_auth,
        ))
        .with_state(srv)
}

/// A desktop request is someone using this gateway (`veles_core::idle`,
/// the phone's `--exit-when-idle`) — except a PASSIVE one, which is the
/// app keeping itself alive, not a person. Two kinds: the notifier's
/// long-poll (`/api/notify/events`, the app listening for asks), and any
/// request the desktop marks `X-Veles-Passive: 1` — a timer firing with
/// no user gesture behind it (the 30 s layout save). An open-but-
/// untouched desktop sends nothing else, so the phone's gateway can stop.
async fn idle_touch(req: axum::extract::Request, next: Next) -> axum::response::Response {
    if !is_passive(&req) {
        let (m, p) = (req.method().clone(), req.uri().path().to_string());
        veles_core::idle::touch_by(|| format!("{m} {p}"));
    }
    next.run(req).await
}

/// Is this desktop request the app talking to itself, not a person? The
/// notifier's long-poll by path; anything else only when the desktop
/// says so — the engine must not grow a list of the desktop's timers,
/// which would drift every time the desktop adds one.
fn is_passive(req: &axum::extract::Request) -> bool {
    // The phone's events: every notification is posted while the engine
    // runs, and one nothing acts on must not keep it up (a turn a
    // trigger starts does that on its own).
    if matches!(req.uri().path(), "/api/notify/events" | "/api/device/event") {
        return true;
    }
    req.headers()
        .get("x-veles-passive")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Every desktop request renders its engine text (command output, the
/// help table, a turn's failover prompt) in the WINDOW's language: the
/// desktop sends `X-Veles-Locale` — its own `::vi18n::current` — on each
/// call. Per request, so the gateway's IRC and Telegram users are not
/// switched along with it. A session with its own `/locale` still wins
/// (`Agent::request_locale`). A malformed header is ignored, not trusted.
async fn locale_scope(req: axum::extract::Request, next: Next) -> axum::response::Response {
    let loc = req
        .headers()
        .get("x-veles-locale")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 32
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'))
        })
        .map(str::to_string);
    veles_core::i18n::scope(loc, next.run(req)).await
}

/// Resolve the desktop API's auth posture, MINTING a token when nothing
/// is configured. Split out of `serve` so the decision is testable
/// without binding a port — and so `serve` hands the router and the bind
/// the same snapshot, which is the invariant that keeps "the middleware
/// wants a token" and "the bind widened" from disagreeing.
///
/// Nothing configured: mint one, rather than serve the mutating half of
/// this API to any process that can reach loopback. Loopback is an
/// address, not a trust boundary — every local user is on it, and a
/// container that publishes the port puts whatever can reach the host
/// there too. Written to `<config_dir>/.env` so it survives this process:
/// a token that changed on every restart would lock out the desktop that
/// remembered it, and the lesson an operator takes from that is to switch
/// the protection off.
fn resolve_auth(port: u16) -> Arc<veles_core::authn::AuthConfig> {
    let mut auth = veles_core::authn::from_env(
        veles_core::authn::DESKTOP_PEER_KEY,
        veles_core::authn::DESKTOP_BEARER_KEY,
    );
    if auth.is_open() {
        if veles_core::authn::desktop_open_opted_out() {
            tracing::warn!(
                "desktop API: {} is set — NO token, open to every process on loopback:{port} \
                 (config writes, live interpreters, ACL edits and sandbox binds included)",
                veles_core::authn::DESKTOP_OPEN_KEY
            );
        } else {
            match veles_core::authn::issue_dotenv_token(veles_core::authn::DESKTOP_BEARER_KEY) {
                Ok(path) => {
                    tracing::info!(
                        "desktop API: no token was configured — minted one into {} (0600). \
                         `veles gui` and `veles tcl` read it from there; any other loopback \
                         client must send it as `Authorization: Bearer …`. Set {}=1 to run \
                         open instead.",
                        path.display(),
                        veles_core::authn::DESKTOP_OPEN_KEY
                    );
                    // Re-resolve so the router, the middleware and the bind
                    // decision all see the token that now exists.
                    auth = veles_core::authn::from_env(
                        veles_core::authn::DESKTOP_PEER_KEY,
                        veles_core::authn::DESKTOP_BEARER_KEY,
                    );
                }
                // Unwritable config is not a reason to refuse to serve: the
                // owner's own desktop still works, and the log says why the
                // posture is what it is.
                Err(e) => {
                    tracing::warn!(
                        "desktop API: could not mint a token ({e:#}) — staying open on loopback"
                    )
                }
            }
        }
    }
    Arc::new(auth)
}

// ── the challenge relay page (PLAN-pool-round.md Part E) ─────────────

const CHALLENGE_PAGE: &str = include_str!("challenge_page.html");

/// The page's words, from the engine catalogs in the gateway's own
/// language (the plan's Step 5.4). Five of these are the desktop
/// challenge dialog's own strings — the relay page IS that dialog, in
/// HTML — so a phone and a window say the same thing in the same
/// language. The two new ones (the placeholder, the 404 note) follow
/// the same English-as-key convention.
///
/// The wire protocol (`click x y`, `type …`, `key Enter`, the
/// `solved`/`give_up`/`window` choices) stays English in the page's
/// JavaScript: `challenge::Action::parse` reads those verbs.
fn challenge_strings() -> Vec<(&'static str, String)> {
    let l = |k: &str| veles_core::i18n::lookup(k, &[]);
    vec![
        ("title", l("Solve this for the agent")),
        (
            "hint",
            l("Click the picture where you would click on the page."),
        ),
        ("type", l("Type")),
        ("type_placeholder", l("text to type")),
        ("solved", l("Solved")),
        ("give_up", l("Give up")),
        ("open_window", l("Open a window")),
        ("over", l("This challenge is over.")),
    ]
}

/// The served page: `{L:strings}` becomes the JS dict and the remaining
/// placeholders their HTML-escaped text. Substituted at serve time, not
/// baked at build time, because the language is the gateway's live one
/// (`/locale primary <id>` rewrites it) and one gateway serves pages in
/// whichever language it speaks.
fn challenge_page_html() -> String {
    let mut out = CHALLENGE_PAGE.to_string();
    let dict: serde_json::Map<String, Value> = challenge_strings()
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v)))
        .collect();
    out = out.replace("{L:strings}", &Value::Object(dict).to_string());
    for (k, v) in challenge_strings() {
        out = out.replace(&format!("{{L:{k}}}"), &html_escape(&v));
    }
    out
}

/// Minimal HTML text escaping for the substituted strings: the catalogs
/// are ours, but a translation containing `<` or `&` must not break the
/// page (or open an injection slot in a page that carries no token
/// beyond its URL).
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn challenge_over() -> Response {
    // The same note the page's own poll shows, in the gateway's
    // language — a person clicking a stale link from a chat reads it.
    (
        StatusCode::NOT_FOUND,
        veles_core::i18n::lookup("This challenge is over.", &[]),
    )
        .into_response()
}

async fn challenge_page(Path(token): Path<String>) -> Response {
    if veles_core::challenge::page(&token).is_none() {
        return challenge_over();
    }
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        challenge_page_html(),
    )
        .into_response()
}

async fn challenge_shot(Path(token): Path<String>) -> Response {
    use base64::Engine as _;
    let Some(p) = veles_core::challenge::page(&token) else {
        return challenge_over();
    };
    match base64::engine::general_purpose::STANDARD.decode(p.image.as_bytes()) {
        Ok(png) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            png,
        )
            .into_response(),
        Err(_) => challenge_over(),
    }
}

async fn challenge_state(Path(token): Path<String>) -> Response {
    match veles_core::challenge::page(&token) {
        Some(p) => Json(json!({
            "round": p.round,
            "prompt": p.prompt,
            "window": p.window_offered,
        }))
        .into_response(),
        None => challenge_over(),
    }
}

async fn challenge_answer(Path(token): Path<String>, Json(body): Json<Value>) -> Response {
    use veles_core::ask::AskAnswer;
    let answer = match (body["choice"].as_str(), body["text"].as_str()) {
        (Some(c @ ("solved" | "give_up" | "window")), _) => AskAnswer::choice(c),
        (None, Some(t)) if veles_core::challenge::Action::parse(t).is_some() => AskAnswer::text(t),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "not something to do on the page" })),
            )
                .into_response()
        }
    };
    if veles_core::challenge::answer_page(&token, answer) {
        Json(json!({ "ok": true })).into_response()
    } else {
        challenge_over()
    }
}

pub async fn serve(srv: Arc<DesktopServer>) -> Result<()> {
    let port = default_port(&srv.cfg());
    // Hand edits of config.toml reach the live handle without a
    // restart (R4.5) — the same watcher tclrt runs for plugins.
    srv.spawn_config_watcher();
    // Background completion wake turns (2026-09-13, USER's contract):
    // the desktop wakes sessions IT owns (not chat-backed ones — the
    // gateway in this process owns those, and the process-wide claim
    // keeps a race between the two owners honest). One loop per
    // server.
    {
        let owner: Arc<dyn veles_core::session_wake::WakeOwner> =
            Arc::new(DesktopWakeOwner { srv: srv.clone() });
        veles_core::rt::spawn(veles_core::session_wake::run_wake_loop(owner));
        // The cron scheduler's serialization seam (Stage 5): a session
        // wakeup borrows the owning surface's turn lock, and for the
        // sessions the desktop holds that surface is this server.
        veles_core::cron::register_session_runner(Arc::new(DesktopSessionRunner {
            srv: srv.clone(),
        }));
        // The same sessions, listed and nudged for session mail.
        veles_core::presence::register(Arc::new(DesktopSessionRunner { srv: srv.clone() }));
    }
    // Resolve the auth config BEFORE the router and the bind, and hand the
    // SAME snapshot to both, so the two decisions can never disagree.
    let auth = resolve_auth(port);
    let config_host = srv
        .cfg()
        .platforms
        .get("desktop")
        .and_then(|d| d.get("extra"))
        .and_then(|e| e.get("host"))
        .and_then(|h| h.as_str())
        .map(str::to_string);
    let app = router(srv, auth.clone());
    // Bind host: loopback by default (the desktop is a single-user local
    // tool). VELES_DESKTOP_HOST widens it for containerized deployment — but
    // only when a token is configured; without one the bind is refused and
    // stays loopback. The published port remains behind SSH / WireGuard /
    // firewall by operator choice.
    let host =
        veles_core::authn::resolve_bind_host("VELES_DESKTOP_HOST", &auth, config_host.as_deref());
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    // Where a challenge's relay page is linked from (Part E): this
    // listener, as the machine itself reaches it — or VELES_CHALLENGE_URL
    // when chat users reach it some other way (a WireGuard address). The
    // read lives in challenge::relay_base_for and is dotenv-aware: on the
    // phone, files/home/.env is the ONLY config channel there is.
    veles_core::challenge::set_relay_base(Some(veles_core::challenge::relay_base_for(port)));
    // Say which mode came up. An auth posture you cannot see in the log is
    // one you find out about the wrong way.
    if auth.is_open() {
        tracing::info!("desktop API: no token configured — open, loopback only");
    } else {
        tracing::info!(
            "desktop API: token auth enabled ({} peer token(s))",
            auth.peer_count()
        );
    }
    tracing::info!("desktop API listening on http://{addr}/");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use veles_core::hooks::HookBus;

    /// A passive request is the app talking to itself, not a person:
    /// the idle clock must not move for it. The notifier's long-poll by
    /// path, the desktop's timers by the header it sends.
    #[test]
    fn a_passive_request_is_not_a_use() {
        let notifier = axum::http::Request::builder()
            .uri("/api/notify/events")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(is_passive(&notifier), "the long-poll is passive by path");
        // The phone posts every notification while the engine runs: one
        // nothing acts on must not keep it up.
        let event = axum::http::Request::builder()
            .uri("/api/device/event")
            .method("POST")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(is_passive(&event), "a phone's event is passive by path");

        let timer = axum::http::Request::builder()
            .uri("/api/desktop/state")
            .method("POST")
            .header("x-veles-passive", "1")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(is_passive(&timer), "a marked timer request is passive");

        // A person tapping Save sends the same path with no marker.
        let person = axum::http::Request::builder()
            .uri("/api/desktop/state")
            .method("POST")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!is_passive(&person), "an unmarked request is a use");

        // Only the exact value marks it — a stray header is not passive.
        let wrong = axum::http::Request::builder()
            .uri("/api/desktop/state")
            .method("POST")
            .header("x-veles-passive", "yes")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!is_passive(&wrong), "only the literal 1 marks passive");
    }

    /// Sessions another node owns are listed — `remote=1`, what the
    /// desktop asks since 2026-10-06 — and their transcripts read from the
    /// replica here, read-only (USER 2026-10-05: the VPS's IRC and Bluesky
    /// sessions "present in the list and lazily-read when opened in
    /// read-only mode on another node … a key audit path"). A session this
    /// node has itself is the local one, listed once; a client that does
    /// not ask sees no change.
    #[test]
    fn a_replicas_sessions_are_listed_and_opened_read_only() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("local-1").unwrap();
        let node = "495908ea-37cd-4bfc-8846-93bf588bc7c8";
        {
            let c = veles_core::storesync::open_replica(node).unwrap();
            c.execute_batch(
                "INSERT INTO sessions(id, title, updated_at)
                     VALUES ('irc-1', '#flashsupport', '2099-01-01 00:00:00');
                 INSERT INTO sessions(id, title, updated_at)
                     VALUES ('local-1', 'taken over', '2099-01-01 00:00:01');
                 INSERT INTO messages(session_id, role, content)
                     VALUES ('irc-1', 'user', 'hello from irc');",
            )
            .unwrap();
        }
        let rt = probe_rt();
        let list = |q: &[(&str, &str)]| -> Value {
            let map: HashMap<String, String> = q
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let (code, Json(v)) =
                rt.block_on(list_sessions(State(srv.clone()), axum::extract::Query(map)));
            assert_eq!(code, StatusCode::OK, "{v}");
            v
        };
        let ids = |v: &Value| -> Vec<String> {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            ids(&list(&[])),
            ["local-1"],
            "an old client sees only local"
        );
        let all = list(&[("remote", "1")]);
        assert_eq!(
            ids(&all),
            ["irc-1", "local-1"],
            "merged, newest first: {all}"
        );
        assert_eq!(all["total"], 2);
        let irc = &all["sessions"][0];
        assert_eq!(irc["remote"], true);
        assert_eq!(irc["node"], node);
        assert!(
            all["sessions"][1].get("remote").is_none(),
            "the local copy wins"
        );

        let (code, Json(h)) = rt.block_on(history(State(srv.clone()), Path("irc-1".into())));
        assert_eq!(code, StatusCode::OK, "{h}");
        assert_eq!(h["read_only"], true, "{h}");
        assert_eq!(h["node"], node);
        assert!(h.to_string().contains("hello from irc"), "{h}");
        let (_, Json(local)) = rt.block_on(history(State(srv.clone()), Path("local-1".into())));
        assert!(
            local.get("read_only").is_none(),
            "a local session is not read-only"
        );
    }

    fn scratch_server() -> (
        tempfile::TempDir,
        Arc<DesktopServer>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        // These build real Agents, and Agent::new bootstraps the memory
        // hierarchy from the AMBIENT VELES_HOME — so without a private
        // one this wrote into another test's scratch store.
        let (home, env) = veles_core::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        // Defaults, never config::load(): loading would read the
        // developer's real config and make the test machine-dependent.
        // A route is still needed — building an Agent resolves the
        // primary route eagerly — so point it at a dead local port. No
        // turn is ever run.
        let cfg = Arc::new(VelesConfig {
            model: veles_core::config::ModelSection {
                provider: Some("test-provider".into()),
                default: Some("test-model".into()),
                ..Default::default()
            },
            custom_providers: vec![
                veles_core::config::ProviderEntry {
                    name: "test-provider".into(),
                    base_url: "http://127.0.0.1:0/v1".into(),
                    api_mode: "chat_completions".into(),
                    // Declared, so set_model's provider search can match
                    // — without a list every name falls to the "assumed"
                    // branch and the test proves nothing.
                    models: veles_core::config::ModelsSpec::List(vec!["test-model".into()]),
                    ..Default::default()
                },
                // A SECOND provider, so an override is distinguishable
                // from the default. With one provider, "reports the
                // override" and "reports the default" give the same
                // answer, and a test cannot tell them apart — a mutation
                // dropping override resolution stayed green on exactly
                // that.
                veles_core::config::ProviderEntry {
                    name: "alt-provider".into(),
                    base_url: "http://127.0.0.1:2/v1".into(),
                    api_mode: "chat_completions".into(),
                    models: veles_core::config::ModelsSpec::List(vec!["alt-model".into()]),
                    ..Default::default()
                },
            ],
            model_aliases: std::collections::HashMap::from([(
                "fast".to_string(),
                veles_core::config::ModelAlias {
                    provider: "alt-provider".into(),
                    model: "alt-model".into(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });
        let hooks = HookBus::new(None);
        let readers = veles_core::store::ReadPool::new(&store.lock_ok());
        let srv = Arc::new(DesktopServer {
            cfg: std::sync::RwLock::new(cfg),
            store,
            readers,
            hooks,
            bus: None,
            scheduler: None,
            agents: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            turn_locks: Mutex::new(HashMap::new()),
            started: Instant::now(),
            desktop_eval: DesktopEvalBus::new(),
            signed_download: crate::desktop_install::SignedDownloads::default(),
        });
        (dir, srv, home, env)
    }

    /// The session list reads on a connection of its own, so a store
    /// lock held elsewhere (a sync apply, a turn's writes) no longer
    /// stalls it. On 2026-10-13 the list itself held that lock for
    /// 30–79 s, and every desktop request queued behind it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the held lock is the test
    async fn the_session_list_reads_beside_a_held_store_lock() {
        let (_d, srv, _h, _e) = scratch_server();
        {
            let st = srv.store.lock_ok();
            st.create_session("s-one").unwrap();
            st.append_message(
                "s-one",
                &veles_core::providers::ChatMessage::user("a question"),
            )
            .unwrap();
        }
        let held = srv.store.lock_ok();
        let listing = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            list_sessions(State(srv.clone()), axum::extract::Query(HashMap::new())),
        )
        .await;
        drop(held);
        let (code, Json(v)) = listing.expect("the list waited for the store lock");
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["total"], 1, "{v}");
        assert_eq!(v["sessions"][0]["id"], "s-one");
        assert_eq!(v["sessions"][0]["preview"], "a question");
        assert_eq!(v["sessions"][0]["message_count"], 1);
    }

    /// What a desktop window reads and polls answers while another holder
    /// keeps the store lock: the transcript and its event stream's replay,
    /// the status line, the Tasks pane, usage, a head, a search, cron and
    /// triggers. A commit waiting seconds on a busy disk held that lock,
    /// and each of these waited behind it on a tokio worker (2026-10-13).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the held lock is the test
    async fn a_desktop_window_reads_beside_a_held_store_lock() {
        let (_d, srv, _h, _e) = scratch_server();
        {
            let st = srv.store.lock_ok();
            st.create_session("s-one").unwrap();
            st.append_message(
                "s-one",
                &veles_core::providers::ChatMessage::user("a needle question"),
            )
            .unwrap();
        }
        let s = || State(srv.clone());
        let id = || Path("s-one".to_string());
        let held = srv.store.lock_ok();
        let reads = async {
            let (_, Json(history)) = history(s(), id()).await;
            let one = HashMap::from([("session".to_string(), "s-one".to_string())]);
            let Json(status) = status(s(), axum::extract::Query(one)).await;
            let Json(todos) = session_todos(s(), id()).await;
            let session = Some("s-one".to_string());
            let Json(usage) = usage(s(), Query(UsageQuery { session })).await;
            let none = axum::extract::Query(HashMap::new());
            let (_, Json(head)) = session_head(s(), id(), none).await;
            let q = SearchQuery {
                q: "needle".into(),
                limit: 5,
            };
            let Json(found) = search(s(), Query(q)).await;
            let Json(cron) = cron_list(s()).await;
            let Json(triggers) = triggers_list(s()).await;
            let from_zero = SessionEventsQuery {
                since: 0,
                since_note: 0,
            };
            let _stream = session_events(s(), id(), Query(from_zero)).await;
            [history, status, todos, usage, head, found, cron, triggers]
        };
        let got = tokio::time::timeout(std::time::Duration::from_secs(20), reads).await;
        drop(held);
        let [history, status, todos, usage, head, found, ..] =
            got.expect("a desktop read waited for the store lock");
        assert!(history["last_msg_id"].as_i64() > Some(0), "{history}");
        assert_eq!(status["session_count"], 1, "{status}");
        assert_eq!(status["plan_mode"], false, "{status}");
        assert!(todos["todos"].is_array(), "{todos}");
        assert!(usage["calls"].is_number(), "{usage}");
        assert_eq!(head["messages"][0]["content"], "a needle question");
        assert_eq!(found["hits"][0]["session_id"], "s-one", "{found}");
    }

    /// The session browser's API (2026-10-04): a page with its total,
    /// pins included past the page, the gateway-side search, a bulk
    /// delete in one request, a session's head, and a branch that is a
    /// real child of its source.
    #[test]
    fn the_session_api_pages_searches_deletes_and_branches() {
        let (_d, srv, _h, _e) = scratch_server();
        {
            let st = srv.store.lock_ok();
            for (id, title, first) in [
                ("s-old", "kept old", "an old question"),
                ("s-a", "", "You are translating ONE page"),
                ("s-b", "", "You are translating ANOTHER page"),
            ] {
                st.create_session(id).unwrap();
                if !title.is_empty() {
                    st.set_title(id, title).unwrap();
                }
                st.append_message(id, &veles_core::providers::ChatMessage::user(first))
                    .unwrap();
            }
            // Distinct stamps: rows made in one second would tie.
            st.conn()
                .execute_batch(
                    "UPDATE sessions SET updated_at = '2000-01-01 00:00:00' WHERE id = 's-old';
                     UPDATE sessions SET updated_at = '2026-01-01 00:00:01' WHERE id = 's-a';
                     UPDATE sessions SET updated_at = '2026-01-01 00:00:02' WHERE id = 's-b';",
                )
                .unwrap();
        }
        let rt = probe_rt();
        let list = |q: &[(&str, &str)]| -> Value {
            let map: HashMap<String, String> = q
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let (code, Json(v)) =
                rt.block_on(list_sessions(State(srv.clone()), axum::extract::Query(map)));
            assert_eq!(code, StatusCode::OK, "{v}");
            v
        };
        let ids = |v: &Value| -> Vec<String> {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_string())
                .collect()
        };
        let page = list(&[("limit", "2")]);
        assert_eq!(page["total"], 3);
        assert_eq!(ids(&page), ["s-b", "s-a"], "newest first, two of three");
        assert_eq!(
            page["sessions"][0]["preview"],
            "You are translating ANOTHER page"
        );
        let pinned = list(&[("limit", "2"), ("include", "s-old")]);
        assert!(
            ids(&pinned).contains(&"s-old".to_string()),
            "a pin past the page"
        );
        assert_eq!(
            ids(&list(&[("q", "another")])),
            ["s-b"],
            "first-line search"
        );
        assert_eq!(ids(&list(&[("titled", "titled")])), ["s-old"]);

        let (code, Json(v)) = rt.block_on(session_head(
            State(srv.clone()),
            Path("s-a".into()),
            axum::extract::Query(HashMap::new()),
        ));
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["messages"][0]["content"], "You are translating ONE page");

        let (code, Json(v)) = rt.block_on(session_branch(State(srv.clone()), Path("s-old".into())));
        assert_eq!(code, StatusCode::OK, "{v}");
        let child = v["new_session_id"].as_str().unwrap().to_string();
        let kids = list(&[("q", "kept old")]);
        let row = kids["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == child.as_str())
            .cloned()
            .expect("the branch is listed");
        assert_eq!(row["parent"], "s-old", "a branch is a child of its source");
        assert_eq!(v["messages_copied"], 1);

        let (code, Json(v)) = rt.block_on(delete_sessions(
            State(srv.clone()),
            Json(json!({"ids": ["s-a", "s-b"]})),
        ));
        assert_eq!((code, v["deleted"].clone()), (StatusCode::OK, json!(2)));
        assert_eq!(list(&[]).get("total").unwrap(), 2, "s-old and its branch");
        let (code, _) = rt.block_on(delete_sessions(State(srv.clone()), Json(json!({}))));
        assert_eq!(code, StatusCode::BAD_REQUEST, "no ids is refused by name");
    }

    /// Build a probe router with the auth middleware and the given
    /// posture. No environment involved: the gate takes its config as
    /// layer state, so a test states the posture it wants directly.
    fn auth_probe_router(cfg: veles_core::authn::AuthConfig) -> axum::Router {
        axum::Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/api/status", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(cfg),
                require_desktop_auth,
            ))
    }

    fn open_cfg() -> veles_core::authn::AuthConfig {
        veles_core::authn::AuthConfig::new(Default::default(), String::new())
    }

    /// The window's language rides on every desktop request and scopes
    /// that request's engine text — per request, never the process (the
    /// gateway's other users keep theirs). A malformed header is ignored.
    #[test]
    fn a_desktop_request_renders_in_the_windows_language() {
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route("/probe", get(|| async { veles_core::i18n::effective() }))
            .layer(middleware::from_fn(locale_scope));
        let rt = probe_rt();
        let ask = |hdr: Option<&str>| -> String {
            let mut req = axum::http::Request::get("/probe")
                .body(axum::body::Body::empty())
                .unwrap();
            if let Some(h) = hdr {
                req.headers_mut()
                    .insert("x-veles-locale", h.parse().unwrap());
            }
            let resp = rt.block_on(app.clone().oneshot(req)).unwrap();
            let bytes = rt
                .block_on(axum::body::to_bytes(resp.into_body(), 1024))
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        };
        assert_eq!(ask(Some("de_DE.UTF-8")), "de");
        assert_eq!(ask(Some("ja")), "ja");
        let process = veles_core::i18n::current();
        assert_eq!(ask(None), process, "no header: the process default");
        assert_eq!(ask(Some("../../etc")), process, "malformed: ignored");
        assert_eq!(
            veles_core::i18n::current(),
            process,
            "never moves the process"
        );
    }

    fn token_cfg(tok: &str) -> veles_core::authn::AuthConfig {
        veles_core::authn::AuthConfig::new(Default::default(), tok.to_string())
    }

    /// A request whose body and method fit the probe routes above.
    /// `peer` becomes the ConnectInfo the real listener would attach.
    fn probe_request(
        path: &str,
        bearer: Option<&str>,
        peer: Option<&str>,
    ) -> axum::http::Request<axum::body::Body> {
        use axum::http::Request;
        let mut b = Request::get(path)
            .body(axum::body::Body::empty())
            .expect("probe request");
        if let Some(t) = bearer {
            b.headers_mut()
                .insert("authorization", format!("Bearer {t}").parse().unwrap());
        }
        if let Some(p) = peer {
            let addr: SocketAddr = p.parse().expect("peer addr");
            b.extensions_mut().insert(ConnectInfo(addr));
        }
        b
    }

    fn probe_status(
        rt: &tokio::runtime::Runtime,
        app: &axum::Router,
        req: axum::http::Request<axum::body::Body>,
    ) -> StatusCode {
        use tower::ServiceExt;
        rt.block_on(app.clone().oneshot(req)).unwrap().status()
    }

    fn probe_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// A3 — the REST half. Four surfaces reach the trigger table and
    /// each validated a different subset: `POST /api/triggers/new`
    /// checked event, action and match but never `min_role`, and
    /// `POST /api/triggers/:name` checked ONLY event, so the desktop's
    /// edit dialog could write `match_kind = "contians"` — which
    /// `into_trigger` reads back as `Always`, widening an image-URL row
    /// to every line in the channel. One validator now, shared with the
    /// chat surfaces.
    #[test]
    fn the_rest_trigger_surfaces_validate_the_same_four_fields() {
        let rt = probe_rt();
        let (_dir, srv, _home, _guard) = scratch_server();
        let bad_values = [
            ("event", "MSG"),
            ("match_kind", "contians"),
            ("action", "visoin"),
            ("min_role", "ownr"),
        ];

        // CREATE refuses each, and writes nothing.
        for (field, bad) in bad_values {
            let mut body = json!({"name": "x", "network": "n", "channel": "#c"});
            body[field] = json!(bad);
            let (code, out) = rt.block_on(trigger_new(State(srv.clone()), Json(body)));
            assert_eq!(code, StatusCode::BAD_REQUEST, "{field}={bad} was accepted");
            let err = out.0["error"].as_str().unwrap_or_default().to_string();
            assert!(err.contains(bad), "the refusal names the value: {err}");
            assert!(
                srv.store.lock_ok().trigger_get("x").unwrap().is_none(),
                "{field}={bad} must not have written a row"
            );
        }

        // A good row lands, canonicalised.
        let (code, _) = rt.block_on(trigger_new(
            State(srv.clone()),
            Json(json!({
                "name": "good", "network": "n", "channel": "#c",
                "match_kind": "ImageURL", "action": "Vision", "min_role": "Trusted",
            })),
        ));
        assert_eq!(code, StatusCode::CREATED);
        let row = srv.store.lock_ok().trigger_get("good").unwrap().unwrap();
        assert_eq!(row.match_kind, "imageurl", "stored canonical, not as typed");
        assert_eq!(row.action, "vision");
        assert_eq!(row.min_role.as_deref(), Some("trusted"));

        // EDIT refuses the same four, and leaves the row alone.
        for (field, bad) in bad_values {
            let (code, out) = rt.block_on(trigger_edit(
                State(srv.clone()),
                Path("good".to_string()),
                Json(json!({ field: bad })),
            ));
            assert_eq!(
                code,
                StatusCode::BAD_REQUEST,
                "edit accepted {field}={bad}: {:?}",
                out.0
            );
        }
        let row = srv.store.lock_ok().trigger_get("good").unwrap().unwrap();
        assert_eq!(row.match_kind, "imageurl", "no refused edit landed");
        assert_eq!(row.action, "vision");
        assert_eq!(row.event, "TEXT");
        assert_eq!(row.min_role.as_deref(), Some("trusted"));

        // `null` still CLEARS the floor — validation must not have
        // turned a legitimate clear into a refusal.
        let (code, _) = rt.block_on(trigger_edit(
            State(srv.clone()),
            Path("good".to_string()),
            Json(json!({ "min_role": null })),
        ));
        assert_eq!(code, StatusCode::OK);
        let row = srv.store.lock_ok().trigger_get("good").unwrap().unwrap();
        assert_eq!(row.min_role, None, "null clears the floor");
    }

    /// The auth gate, pinned: no token ⇒ loopback pass-through; token ⇒
    /// 401 without/wrong bearer, 200 with it, health always open.
    #[test]
    fn desktop_auth_gate_enforced_when_token_configured() {
        let rt = probe_rt();

        // No token configured → a loopback caller passes with no header.
        let app = auth_probe_router(open_cfg());
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/status", None, Some("127.0.0.1:5000"))
            ),
            StatusCode::OK,
            "loopback in open mode must be open"
        );

        // Token configured → the gate closes.
        let app = auth_probe_router(token_cfg("secret"));
        assert_eq!(
            probe_status(&rt, &app, probe_request("/api/status", None, None)),
            StatusCode::UNAUTHORIZED,
            "no header must 401"
        );
        assert_eq!(
            probe_status(&rt, &app, probe_request("/api/status", Some("wrong"), None)),
            StatusCode::UNAUTHORIZED,
            "wrong token must 401"
        );
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/status", Some("secret"), None)
            ),
            StatusCode::OK,
            "correct token must pass"
        );
        assert_eq!(
            probe_status(&rt, &app, probe_request("/api/health", None, None)),
            StatusCode::OK,
            "health must stay unauthenticated"
        );
    }

    /// Open mode is LOOPBACK mode, and the code says so rather than
    /// leaving it to the bind. `resolve_bind_host` already refuses to
    /// widen without a token, so a non-loopback peer arriving in open mode
    /// means something else put the server on the network — a reverse
    /// proxy, socket activation, a port forward. Refuse it there too.
    ///
    /// Mutation check: drop the `is_loopback` guard and this goes red
    /// while every other assertion stays green.
    #[test]
    fn open_mode_refuses_a_non_loopback_peer() {
        let rt = probe_rt();
        let app = auth_probe_router(open_cfg());
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/status", None, Some("127.0.0.1:5000"))
            ),
            StatusCode::OK
        );
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/status", None, Some("[::1]:5000"))
            ),
            StatusCode::OK,
            "IPv6 loopback counts as loopback"
        );
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/status", None, Some("10.0.0.7:5000"))
            ),
            StatusCode::UNAUTHORIZED,
            "an off-host caller must not reach an unauthenticated API"
        );
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/health", None, Some("10.0.0.7:5000"))
            ),
            StatusCode::OK,
            "health stays reachable — it is the container liveness probe"
        );
    }

    /// In TOKEN mode the address is a label, not a gate: a roaming client
    /// changes address constantly and must not be dropped at handover.
    #[test]
    fn token_mode_admits_any_peer_address() {
        let rt = probe_rt();
        let app = auth_probe_router(token_cfg("roam"));
        for peer in ["10.0.0.2:1", "203.0.113.7:2", "[2001:db8::1]:3"] {
            assert_eq!(
                probe_status(
                    &rt,
                    &app,
                    probe_request("/api/status", Some("roam"), Some(peer))
                ),
                StatusCode::OK,
                "{peer} presented a valid token"
            );
        }
    }

    // ── the default posture: a token exists ──────────────────────────

    /// An unconfigured desktop API MINTS its own token on serve, into
    /// config's `.env`, and hands the router the snapshot that requires
    /// it.
    ///
    /// Loopback is an address, not a trust boundary, and this route table
    /// holds config writes, two live interpreters, ACL edits and sandbox
    /// binds. The gate had existed for a round with nothing ever turning
    /// it on: `AuthConfig::from_env` on a default install resolves open,
    /// and "no token configured" was the shipped default.
    #[test]
    fn an_unconfigured_desktop_api_mints_its_own_token() {
        let (_cfg_root, _data_root, _guard) = veles_core::testenv::scratch_xdg();
        std::env::remove_var(veles_core::authn::DESKTOP_BEARER_KEY);
        std::env::remove_var(veles_core::authn::DESKTOP_PEER_KEY);
        std::env::remove_var(veles_core::authn::DESKTOP_OPEN_KEY);

        let auth = resolve_auth(8642);
        assert!(!auth.is_open(), "the default posture must not be open");

        // The minted credential is the one the middleware accepts, and the
        // one a client reading the same file will send.
        let token = veles_core::authn::desktop_bearer_token();
        assert_eq!(token.len(), 64, "two v4 UUIDs, hex: {token:?}");
        let rt = probe_rt();
        let app = auth_probe_router((*auth).clone());
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/status", Some(&token), Some("127.0.0.1:0"))
            ),
            StatusCode::OK,
            "the minted token must be accepted by the gate it was minted for"
        );
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/status", None, Some("127.0.0.1:0"))
            ),
            StatusCode::UNAUTHORIZED,
            "…and a tokenless loopback caller must not be"
        );
        assert_eq!(
            probe_status(
                &rt,
                &app,
                probe_request("/api/health", None, Some("127.0.0.1:0"))
            ),
            StatusCode::OK,
            "health stays open on purpose — it is how a launcher finds the gateway"
        );

        // The next boot keeps the SAME token. One that rotated on every
        // restart would lock out the desktop that remembered it, and the
        // lesson an operator takes from that is to switch the protection
        // off.
        let again = resolve_auth(8642);
        assert!(
            !again.is_open(),
            "a restart must resolve the token it minted"
        );
        assert_eq!(
            veles_core::authn::desktop_bearer_token(),
            token,
            "a restart must not rotate the token"
        );

        // Where it went: config's `.env` (the file every other key is read
        // from), owner-only.
        let path = veles_core::config::config_dir().join(".env");
        let body = std::fs::read_to_string(&path).expect("the .env exists");
        assert!(
            body.contains(&format!(
                "{}={token}",
                veles_core::authn::DESKTOP_BEARER_KEY
            )),
            "the token must be readable from {path:?}: {body}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a local token for every local user: {mode:o}");
        }
    }

    /// A token the operator configured is respected rather than replaced,
    /// and named peers alone are a posture — minting a shared token
    /// beside them would add a credential nobody asked for.
    #[test]
    fn a_configured_token_or_peer_list_mints_nothing() {
        let (_c, _d, _guard) = veles_core::testenv::scratch_xdg();
        std::env::remove_var(veles_core::authn::DESKTOP_OPEN_KEY);
        std::env::remove_var(veles_core::authn::DESKTOP_PEER_KEY);

        std::env::set_var(veles_core::authn::DESKTOP_BEARER_KEY, "operator-mine");
        let auth = resolve_auth(8642);
        assert!(!auth.is_open());
        assert_eq!(
            veles_core::authn::desktop_bearer_token(),
            "operator-mine",
            "an exported token wins and is not rotated"
        );
        std::env::remove_var(veles_core::authn::DESKTOP_BEARER_KEY);

        std::env::set_var(veles_core::authn::DESKTOP_PEER_KEY, "laptop:peer-tok");
        let auth = resolve_auth(8642);
        assert!(!auth.is_open(), "peer tokens are a configured posture");
        assert_eq!(auth.peer_count(), 1);
        assert!(
            veles_core::authn::desktop_bearer_token().is_empty(),
            "no shared token is minted beside a peer list"
        );
        std::env::remove_var(veles_core::authn::DESKTOP_PEER_KEY);
    }

    /// The escape hatch, and the half that keeps it from being a hole:
    /// `VELES_DESKTOP_OPEN=1` writes nothing and stays open, and open
    /// still means LOOPBACK — the bind refuses to widen without a
    /// credential.
    #[test]
    fn the_open_opt_out_mints_nothing_and_still_will_not_widen() {
        let (_c, _d, _guard) = veles_core::testenv::scratch_xdg();
        std::env::remove_var(veles_core::authn::DESKTOP_BEARER_KEY);
        std::env::remove_var(veles_core::authn::DESKTOP_PEER_KEY);
        std::env::set_var(veles_core::authn::DESKTOP_OPEN_KEY, "1");

        let auth = resolve_auth(8642);
        assert!(auth.is_open(), "the opt-out keeps the historical posture");
        assert!(
            veles_core::authn::desktop_bearer_token().is_empty(),
            "and mints nothing"
        );
        assert!(
            !veles_core::config::config_dir().join(".env").exists(),
            "no .env is written for an operator who declined one"
        );

        std::env::set_var("VELES_DESKTOP_HOST", "0.0.0.0");
        assert_eq!(
            veles_core::authn::resolve_bind_host("VELES_DESKTOP_HOST", &auth, Some("10.0.0.1")),
            "127.0.0.1",
            "open and reachable from the network is the one combination that must stay refused"
        );
        std::env::remove_var("VELES_DESKTOP_HOST");
        std::env::remove_var(veles_core::authn::DESKTOP_OPEN_KEY);
    }

    /// The Aug 22 regression, pinned.
    ///
    /// `guards` used to call `blocking_lock()` on the per-agent tokio
    /// Mutex from inside this async handler. Whenever a turn held the
    /// agent that panicked with "Cannot block the current thread from
    /// within a runtime", which poisoned the store mutex behind it — and
    /// then every later `lock().unwrap()` anywhere in the process
    /// panicked on the poison until someone restarted the gateway.
    /// journald caught the whole chain: one blocking_lock panic, then
    /// nine PoisonError panics.
    ///
    /// A held agent must therefore report "busy", not bring the endpoint
    /// (or its neighbours) down.
    #[tokio::test]
    async fn guards_reports_busy_instead_of_panicking_on_a_held_agent() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();

        // A REAL session with a REAL turn registered. `session_tap` is
        // what `run_turn` populates for every surface, so this is the
        // same state a live turn leaves behind — and unlike the held
        // agent mutex it used to be, it cannot be confused with a
        // second reader.
        let _agent = srv.agent_for("s1").expect("agent for a real session");
        veles_core::session_tap::turn_started("s1", "desktop", 0, "");

        let Json(v) = guards(State(srv.clone())).await;
        assert_eq!(v["status"], "ok", "guards must answer, not panic: {v}");
        assert_eq!(
            v["active_sessions"][0]["state"], "busy (turn in progress)",
            "a held agent is reported busy, not awaited: {v}"
        );

        // And the store lock is still usable afterwards — the poison
        // domino started exactly here.
        assert!(srv.store.lock_ok().session_exists("s1").unwrap());
    }

    /// A turn started by ANOTHER surface makes the session read busy on
    /// the desktop.
    ///
    /// It did not, and the reason was structural: `busy` came from
    /// `try_lock` on the desktop's OWN cached `Agent`, and there are three
    /// Agent caches in this process. A turn started from IRC, telegram or
    /// cron ran against a different instance, so this endpoint cheerfully
    /// reported the session idle while it was mid-turn — and a client
    /// that believes a busy session is idle is a client that will start a
    /// second turn on it.
    #[tokio::test]
    async fn a_turn_from_another_surface_reads_busy_here() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "cross-surface-busy";
        srv.store.lock_ok().create_session(sid).unwrap();

        let (_c, Json(idle)) = context_view(State(srv.clone()), Path(sid.to_string())).await;
        assert_eq!(idle["busy"], false, "nothing running yet: {idle}");

        // Started elsewhere — note the desktop's Agent is NOT locked, so
        // the old try_lock test would have said "idle".
        veles_core::session_tap::turn_started(sid, "irc", 0, "");
        let (_c, Json(busy)) = context_view(State(srv.clone()), Path(sid.to_string())).await;
        assert_eq!(busy["busy"], true, "a turn is running: {busy}");
        assert_eq!(
            busy["turn_surface"], "irc",
            "and the client is told whose turn it is: {busy}"
        );

        veles_core::session_tap::turn_ended(sid);
        let (_c, Json(done)) = context_view(State(srv.clone()), Path(sid.to_string())).await;
        assert_eq!(done["busy"], false, "and idle again after: {done}");
    }

    #[tokio::test]
    async fn health_and_status_answer_without_a_gateway_bus() {
        let (_d, srv, _home, _env) = scratch_server();
        let Json(h) = health(State(srv.clone())).await;
        assert_eq!(h["status"], "ok");

        // bus: None is the desktop-only configuration; status must not
        // assume an attached gateway.
        let Json(s) = status(State(srv.clone()), axum::extract::Query(Default::default())).await;
        assert_eq!(s["status"], "ok");
        assert_eq!(
            s["platforms"].as_array().map(|a| a.len()),
            Some(0),
            "no bus attached means no platforms, not a panic"
        );
        // Plan mode is per session, so with no `?session=` it is NULL
        // rather than false — "false" would be a claim about a session
        // nobody asked about.
        assert!(
            s["plan_mode"].is_null(),
            "unscoped status must not guess: {}",
            s["plan_mode"]
        );

        // …and with a session it answers for THAT one.
        let mut q = std::collections::HashMap::new();
        q.insert("session".to_string(), "plan-status-sess".to_string());
        {
            let st = srv.store.lock().unwrap();
            veles_core::plan::set_mode(&st, "plan-status-sess", veles_core::plan::Mode::Plan)
                .unwrap();
        }
        let Json(s) = status(State(srv.clone()), axum::extract::Query(q.clone())).await;
        assert_eq!(s["plan_mode"], serde_json::json!(true));
        q.insert("session".to_string(), "some-other-sess".to_string());
        let Json(s) = status(State(srv.clone()), axum::extract::Query(q)).await;
        assert_eq!(
            s["plan_mode"],
            serde_json::json!(false),
            "one session's mode is its own"
        );
    }

    /// Seed a skill in the scratch home so the selection tests have a
    /// real library to switch on and off.
    fn seed_skill(home: &tempfile::TempDir, name: &str, description: &str) {
        let dir = home.path().join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nbody\n"),
        )
        .expect("SKILL.md");
    }

    /// RM3-S4/A4: a persisted injection row is served with its class
    /// AND the mute knob for it, so the transcript can label the block
    /// and offer the control — steer and compaction get the label but
    /// no knob, because the user's own words and the structural
    /// Every history row says when it happened — RFC 3339 UTC, the
    /// store's own time — so the desktop can stamp it (USER 2026-09-29:
    /// "it's not always obvious to me from the context when the messages
    /// were sent"). Notes too, and a resumed read keeps the pairing.
    #[tokio::test]
    async fn history_rows_say_when_they_happened() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "stamped-rows";
        srv.store.lock_ok().create_session(sid).expect("create");
        let first = srv
            .store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("one"))
            .expect("one");
        srv.store
            .lock_ok()
            .conn()
            .execute(
                "UPDATE messages SET created_at = '2026-09-28 23:58:10' WHERE msg_id = ?1",
                [first],
            )
            .expect("backdate");
        srv.store
            .lock_ok()
            .record_note(sid, "guard", "a guard fired", None, None)
            .expect("note");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("two"))
            .expect("two");
        let rows = history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, 0, 0).expect("rows");
        let one = rows.iter().find(|r| r["content"] == "one").expect("one");
        assert_eq!(one["msg_id"], json!(first), "{one}");
        assert_eq!(one["at"], json!("2026-09-28T23:58:10Z"), "{one}");
        let note = rows.iter().find(|r| r["role"] == "note").expect("note");
        assert!(
            note["at"]
                .as_str()
                .is_some_and(|t| t.ends_with('Z') && t.contains('T')),
            "{note}"
        );
        // Resumed past the first row: the times still line up with their rows.
        let after =
            history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, first, 0).expect("after");
        let two = after.iter().find(|r| r["content"] == "two").expect("two");
        assert!(
            two["at"]
                .as_str()
                .is_some_and(|t| t != "2026-09-28T23:58:10Z"),
            "{two}"
        );
    }

    /// summary are never muteable.
    #[tokio::test]
    async fn injection_rows_carry_class_and_mute_knob() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "injection-rows";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(
                sid,
                &veles_core::providers::ChatMessage::user("[BACKGROUND SHELL COMPLETE] x")
                    .with_injection("background"),
            )
            .expect("background");
        srv.store
            .lock_ok()
            .append_message(
                sid,
                &veles_core::providers::ChatMessage::user("[STEER] use python")
                    .with_injection("steer"),
            )
            .expect("steer");

        let rows = history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, 0, 0).expect("rows");
        let background = rows
            .iter()
            .find(|r| r["content"].as_str() == Some("[BACKGROUND SHELL COMPLETE] x"))
            .expect("background row");
        assert_eq!(background["injection"], json!("background"));
        assert_eq!(
            background["toggle"]["key"],
            json!("agent.injections.background"),
            "the muteable class carries its knob: {background}"
        );
        assert_eq!(
            background["toggle"]["value"],
            json!("true"),
            "default is on, stated: {background}"
        );

        let steer = rows
            .iter()
            .find(|r| r["content"].as_str() == Some("[STEER] use python"))
            .expect("steer row");
        assert_eq!(steer["injection"], json!("steer"));
        assert!(
            steer.get("toggle").is_none(),
            "steer is the user's own words — no mute knob: {steer}"
        );
    }

    /// The full load and a resumed stream's replay come from ONE
    /// renderer, so a client that reconnects sees the tail of exactly
    /// what a client that never left is looking at.
    ///
    /// The bug this guards against is structural, not arithmetic: the
    /// live path and the reload path used to be different code, and
    /// every difference between them turned into a user report.
    #[tokio::test]
    async fn a_resumed_stream_replays_only_what_came_after_the_cursor() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "replay-cursor";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("first"))
            .expect("m1");
        let cursor = srv.store.lock_ok().last_msg_id(sid).expect("cursor");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("second"))
            .expect("m2");

        let all = history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, 0, 0).expect("full");
        let after = history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, cursor, 0)
            .expect("resumed");

        let text = |rows: &[Value]| -> Vec<String> {
            rows.iter()
                .map(|r| r["content"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(text(&all), vec!["first", "second"], "full load: {all:?}");
        assert_eq!(
            text(&after),
            vec!["second"],
            "a resume must not re-send what the client already has: {after:?}"
        );

        // And the cursor the client is handed actually advances, so a
        // second resume from it returns nothing rather than looping.
        let head = srv.store.lock_ok().last_msg_id(sid).expect("head");
        assert!(head > cursor, "the cursor moved: {head} > {cursor}");
        assert!(
            history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, head, 0)
                .expect("caught up")
                .is_empty(),
            "a client that is caught up gets nothing"
        );
    }

    /// The replay side of desktop prompt identity: an optimistic row and
    /// its store row must be joinable without comparing their text.
    #[tokio::test]
    async fn replayed_user_history_carries_the_store_row_id() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "history-prompt-identity";
        let store = srv.store.lock_ok();
        store.create_session(sid).unwrap();
        let user_id = store
            .append_message(sid, &veles_core::providers::ChatMessage::user("same words"))
            .unwrap();
        store
            .append_message(
                sid,
                &veles_core::providers::ChatMessage::assistant("an answer", None),
            )
            .unwrap();
        let rows = history_rows(&store, &srv.cfg.read_ok(), sid, 0, 0).expect("history");
        assert_eq!(rows[0]["msg_id"], json!(user_id));
        assert_eq!(rows[0]["content"], json!("same words"));
        assert!(
            rows[1].get("msg_id").is_none(),
            "only the prompt needs this identity"
        );
        let caught_up = history_rows(&store, &srv.cfg.read_ok(), sid, user_id, 0).expect("resume");
        assert_eq!(
            caught_up.len(),
            1,
            "the already-painted prompt is not resent"
        );
        assert_eq!(caught_up[0]["role"], json!("assistant"));
    }

    /// A turn started by ANY surface is observable, and the snapshot
    /// carries the text streamed before the watcher arrived.
    ///
    /// The desktop used to read liveness off its own cached `Agent`, so a
    /// turn started from IRC, telegram or cron did not show as busy at
    /// all — three agent caches, three different answers.
    #[tokio::test]
    async fn a_watcher_sees_a_turn_it_did_not_start() {
        let sid = "watcher-sees-irc-turn";
        let base = 7;
        veles_core::session_tap::turn_started(sid, "irc", base, "");
        veles_core::session_tap::publish_stream(
            sid,
            &veles_core::providers::StreamEvent::Delta("half a sentence".into()),
        );

        let snap = veles_core::session_tap::snapshot(sid).expect("the turn is visible");
        assert_eq!(snap.surface, "irc");
        assert_eq!(
            snap.base_msg_id, base,
            "where the persisted transcript ends"
        );
        assert_eq!(snap.content, "half a sentence");

        veles_core::session_tap::turn_ended(sid);
        assert!(veles_core::session_tap::snapshot(sid).is_none());
    }

    /// A client attaching mid-turn gets BOTH the store's replay and the
    /// snapshot of the turn in flight. Every round has to arrive exactly
    /// once across the two.
    ///
    /// The agent persists a round's rows AS IT GOES (`push_persist`,
    /// right after each ToolEnd), and the snapshot keeps every round of
    /// the turn — so the replay from a pre-turn cursor carried the
    /// finished rounds and the snapshot carried them again. The desktop
    /// painted both, and `turn_anchor` is set after the replay, so the
    /// turn's own reconcile kept the pair: a mid-turn reconnect doubled
    /// every ⚙ line the turn had already made.
    ///
    /// `base_msg_id` exists to prevent exactly that — "a client
    /// attaching mid-turn replays the store up to it and takes
    /// everything after from the snapshot" (agent.rs) — and no client
    /// ever read it. The endpoint holds both halves, so it is the one
    /// place that can honour it for every client at once.
    #[tokio::test]
    async fn a_mid_turn_attach_is_not_handed_rounds_the_store_already_has() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "snapshot-trim";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("run it"))
            .expect("the question");
        // Where the persisted transcript stood when the turn began — and
        // the client's cursor, because it painted history up to here.
        let base = srv.store.lock_ok().last_msg_id(sid).expect("head");
        veles_core::session_tap::turn_started(sid, "desktop", base, "");

        // ROUND ONE: called, finished, and persisted — the turn loop's
        // own order (ToolEnd, then push_persist).
        veles_core::session_tap::publish_stream(
            sid,
            &veles_core::providers::StreamEvent::ToolStart("read_file".into(), "{}".into()),
        );
        veles_core::session_tap::publish_stream(
            sid,
            &veles_core::providers::StreamEvent::ToolEnd("read_file".into(), "contents".into()),
        );
        {
            let store = srv.store.lock_ok();
            let mut call = veles_core::providers::ChatMessage::assistant("", None);
            call.tool_calls = Some(serde_json::json!([{
                "id": "c1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{}"}
            }]));
            store.append_message(sid, &call).expect("the call row");
            store
                .append_message(
                    sid,
                    &veles_core::providers::ChatMessage::tool_result(
                        "c1",
                        "read_file",
                        "contents".into(),
                    ),
                )
                .expect("the result row");
        }
        // ROUND TWO: in flight. Nothing about it is in the store, so the
        // snapshot is the only place it can come from.
        veles_core::session_tap::publish_stream(
            sid,
            &veles_core::providers::StreamEvent::ToolStart("terminal".into(), "ls".into()),
        );

        assert_eq!(
            veles_core::session_tap::snapshot(sid)
                .expect("live turn")
                .rounds
                .len(),
            2,
            "the tap itself keeps both rounds — that is not the bug"
        );

        let sse = session_events(
            State(srv.clone()),
            Path(sid.to_string()),
            Query(SessionEventsQuery {
                since: base,
                since_note: 0,
            }),
        )
        .await;

        // Read up to the live boundary: the replay, then the snapshot.
        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut replay: Vec<Value> = Vec::new();
        let mut snap: Option<Value> = None;
        let mut live = false;
        while !live {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
                .await
                .expect("frames within 5s")
                .expect("body still open")
                .expect("bytes");
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                let mut kind = "";
                let mut data = "";
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = v.trim();
                    }
                }
                match kind {
                    "history" => replay.push(serde_json::from_str(data).expect("row json")),
                    "turn-snapshot" => snap = Some(serde_json::from_str(data).expect("snap json")),
                    "session-live" => live = true,
                    _ => {}
                }
            }
        }

        let replayed_tools: Vec<&str> = replay
            .iter()
            .filter(|r| r["role"] == "tool")
            .map(|r| r["tool_name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            replayed_tools,
            vec!["read_file"],
            "the store's own copy of round one, replayed: {replay:?}"
        );

        let snap = snap.expect("a turn is in flight, so a snapshot rides with the replay");
        let rounds = snap["rounds"].as_array().expect("rounds");
        let names: Vec<&str> = rounds
            .iter()
            .map(|r| r["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            vec!["terminal"],
            "only the round the store does not have: {rounds:?}"
        );
        assert_eq!(
            snap["base_msg_id"].as_i64(),
            Some(base),
            "…and it still says where the persisted transcript ended"
        );

        veles_core::session_tap::turn_ended(sid);
    }

    /// The core of "manage which skills are injected per session": the
    /// toggle has to change what the CURRENT session's prompt says, not
    /// only the next one's. The index is a frozen snapshot, so this is
    /// the assertion that the rebuild actually happens.
    #[tokio::test]
    async fn a_session_skill_toggle_rewrites_that_sessions_prompt() {
        let (_d, srv, home, _env) = scratch_server();
        seed_skill(&home, "widget-polishing", "Use when polishing widgets.");
        let sid = "skill-toggle";
        srv.store.lock_ok().create_session(sid).expect("create");

        let agent = srv.agent_for(sid).expect("agent");
        assert!(
            agent.system_prompt.read_ok().contains("widget-polishing"),
            "the skill starts out in the index"
        );

        let (code, Json(body)) = session_skill(
            State(srv.clone()),
            Path(sid.to_string()),
            Json(json!({ "name": "widget-polishing", "value": "off" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(
            !agent.system_prompt.read_ok().contains("widget-polishing"),
            "switching it off must rebuild the prompt without it"
        );

        // …and back, so the override is a switch rather than a delete.
        let (_, _) = session_skill(
            State(srv.clone()),
            Path(sid.to_string()),
            Json(json!({ "name": "widget-polishing", "value": "default" })),
        )
        .await;
        assert!(agent.system_prompt.read_ok().contains("widget-polishing"));
    }

    /// A skill switched off for a session must be off for the TOOLS too.
    /// An agent that can still `skill_view` it has not had it switched
    /// off, it has just had it hidden from the index.
    #[tokio::test]
    async fn a_disabled_skill_is_hidden_from_the_skill_tools_too() {
        use veles_core::tools::Tool;
        let (_d, srv, home, _env) = scratch_server();
        seed_skill(&home, "widget-polishing", "Use when polishing widgets.");
        let sid = "skill-tools";
        srv.store.lock_ok().create_session(sid).expect("create");
        let _ = srv.agent_for(sid).expect("agent");

        let list = veles_core::skills::SkillsListTool {
            session_id: sid.to_string(),
            store: srv.store.clone(),
        };
        assert!(list.call(json!({})).await.contains("widget-polishing"));

        let (_, Json(body)) = session_skill(
            State(srv.clone()),
            Path(sid.to_string()),
            Json(json!({ "name": "widget-polishing", "value": "off" })),
        )
        .await;
        assert!(body["message"].is_string(), "{body}");

        assert!(
            !list.call(json!({})).await.contains("widget-polishing"),
            "skills_list must respect the session's selection"
        );
        let view = veles_core::skills::SkillViewTool {
            session_id: sid.to_string(),
            store: srv.store.clone(),
        };
        let answer = view.call(json!({ "name": "widget-polishing" })).await;
        assert!(
            answer.contains("switched off for this session"),
            "and say WHY, not 'not found': {answer}"
        );
    }

    /// A switched-off skill still has to be listed, with its state — the
    /// same rule the plugins list needed.
    #[tokio::test]
    async fn skills_list_reports_state_for_every_skill() {
        let (_d, srv, home, _env) = scratch_server();
        seed_skill(&home, "kept", "Use when keeping.");
        seed_skill(&home, "dropped", "Use when dropping.");
        std::fs::write(
            home.path().join("config.toml"),
            "[skills]\ndisabled = [\"dropped\"]\n",
        )
        .expect("config");

        let Json(body) = skills_list(State(srv), Query(StdMap::new())).await;
        let skills = body["skills"].as_array().expect("skills");
        // Assert on the seeded pair, never on the total: skill_roots()
        // also walks ~/.hermes/skills off the REAL home (migration
        // compat, deliberately not under VELES_HOME), so a count here
        // would pass or fail depending on whose machine ran it.
        let find = |n: &str| {
            skills
                .iter()
                .find(|s| s["name"] == json!(n))
                .unwrap_or_else(|| panic!("{n} must be listed: {body}"))
        };
        let kept = find("kept");
        assert_eq!(kept["enabled"], json!(true), "{body}");
        assert_eq!(kept["disabled_by_default"], json!(false), "{body}");

        let dropped = find("dropped");
        assert_eq!(
            dropped["enabled"],
            json!(false),
            "a globally disabled skill is still LISTED, just off: {body}"
        );
        assert_eq!(dropped["disabled_by_default"], json!(true), "{body}");
        assert_eq!(dropped["editable"], json!(true), "veles-owned: {body}");
        assert!(
            dropped["session_override"].is_null(),
            "off by config is not the same as off by session: {body}"
        );
    }

    /// New skills must be usable, not just present: a body with no
    /// frontmatter description is invisible to the index, so creating
    /// one without a template would produce a skill that does nothing.
    #[tokio::test]
    async fn a_new_skill_is_created_with_a_usable_body() {
        let (_d, srv, home, _env) = scratch_server();
        let (code, Json(body)) = skill_new(Json(json!({
            "name": "fresh-skill",
            "description": "Use when testing freshness.",
        })))
        .await;
        assert_eq!(code, StatusCode::CREATED, "{body}");
        let path = home.path().join("skills/fresh-skill/SKILL.md");
        let text = std::fs::read_to_string(&path).expect("written");
        assert!(
            text.contains("description: Use when testing freshness."),
            "{text}"
        );

        let Json(listed) = skills_list(State(srv), Query(StdMap::new())).await;
        assert!(
            listed["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == json!("fresh-skill")),
            "and it is discoverable straight away: {listed}"
        );
    }

    /// The two refusals that keep an agent out of the running window are
    /// the whole safety story for app-target evals, so they are asserted
    /// rather than assumed: nobody listening, and listening but not
    /// armed. Both must answer immediately — waiting 20s for a timeout
    /// would be indistinguishable from a hung window.
    #[tokio::test]
    async fn an_app_eval_is_refused_when_nothing_is_listening_or_it_is_not_armed() {
        let (_d, srv, _home, _env) = scratch_server();

        let (code, Json(body)) = desktop_eval(
            State(srv.clone()),
            Json(json!({ "code": "expr 1+1", "target": "app" })),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            body["result"]
                .as_str()
                .unwrap()
                .contains("no desktop window"),
            "{body}"
        );

        // Now something IS listening, but live patching is off.
        let _sub = srv.desktop_eval.requests.subscribe();
        let (code, Json(body)) = desktop_eval(
            State(srv.clone()),
            Json(json!({ "code": "expr 1+1", "target": "app" })),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{body}");
        assert!(
            body["result"].as_str().unwrap().contains("not armed"),
            "and it says how to fix it: {body}"
        );
    }

    /// The round trip: a request reaches the subscriber, the answer
    /// posted back resolves the waiting call.
    #[tokio::test]
    async fn an_armed_app_eval_round_trips_through_the_desktop() {
        let (_d, srv, _home, _env) = scratch_server();
        let mut sub = srv.desktop_eval.requests.subscribe();
        // A second subscription stands in for the window staying open
        // after this request. Without it the disarm check at the end
        // reports "nothing is listening" (correctly — the answering task
        // owns the only receiver and drops it when it finishes), which
        // is a different refusal from the one being tested.
        let _still_open = srv.desktop_eval.requests.subscribe();
        let (_, Json(armed)) =
            desktop_eval_arm(State(srv.clone()), Json(json!({ "armed": true }))).await;
        assert_eq!(armed["armed"], json!(true));

        // Stand in for the desktop: take the request, answer it.
        let answering = {
            let srv = srv.clone();
            veles_core::rt::spawn(async move {
                let req = sub.recv().await.expect("a request reaches the desktop");
                let id = req["id"].as_str().expect("correlation id").to_string();
                assert_eq!(req["code"], json!("expr 1+1"));
                desktop_eval_result(
                    State(srv),
                    Json(json!({ "id": id, "ok": true, "result": "2" })),
                )
                .await
            })
        };

        let (code, Json(body)) = desktop_eval(
            State(srv.clone()),
            Json(json!({ "code": "expr 1+1", "target": "app" })),
        )
        .await;
        let _ = answering.await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], json!(true), "{body}");
        assert_eq!(body["result"], json!("2"), "{body}");

        // Disarming must actually close the door again.
        let _ = desktop_eval_arm(State(srv.clone()), Json(json!({ "armed": false }))).await;
        let (code, _) = desktop_eval(
            State(srv.clone()),
            Json(json!({ "code": "expr 1+1", "target": "app" })),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
    }

    /// A schedule that will never fire is refused at creation, not
    /// discovered by waiting for a job that silently does nothing.
    #[tokio::test]
    async fn a_cron_job_is_created_edited_and_deleted() {
        let (_d, srv, _home, _env) = scratch_server();

        let (code, Json(bad)) = cron_new(
            State(srv.clone()),
            Json(json!({ "prompt": "check the thing", "schedule": "whenever" })),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");

        let (code, Json(job)) = cron_new(
            State(srv.clone()),
            Json(json!({
                "prompt": "check the thing",
                "schedule": "every 1h",
                "name": "thing-check",
            })),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{job}");
        let id = job["id"].as_str().expect("id").to_string();
        assert!(
            job["next_run_at"].is_string(),
            "the first fire is computed: {job}"
        );
        assert!(
            !job["schedule_human"].as_str().unwrap_or("").is_empty(),
            "the row explains its own schedule: {job}"
        );

        let (code, Json(edited)) = cron_edit(
            State(srv.clone()),
            Path(id.clone()),
            Json(json!({ "prompt": "check the other thing" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{edited}");
        assert_eq!(edited["prompt"], json!("check the other thing"));
        assert_eq!(
            edited["name"],
            json!("thing-check"),
            "an absent field is left alone, not cleared: {edited}"
        );

        let (code, _) = cron_delete(State(srv.clone()), Path(id.clone())).await;
        assert_eq!(code, StatusCode::OK);
        let (code, _) = cron_delete(State(srv.clone()), Path(id)).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "deleting twice is a 404");
    }

    /// The memory budget is why the agent's memory tool refuses writes.
    /// The UI path has to refuse the same way, or the overrun surfaces
    /// at the next prompt build instead of at the edit.
    #[tokio::test]
    async fn memory_write_round_trips_and_respects_the_budget() {
        let (_d, srv, _home, _env) = scratch_server();

        let (code, Json(body)) = memory_write(Json(json!({
            "store": "memory",
            "entries": ["first thing", "second thing"],
        })))
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["written"], json!(2), "{body}");

        let Json(view) = memory_view(State(srv)).await;
        let entries = view["memory"].as_array().expect("entries");
        assert_eq!(entries.len(), 2, "{view}");

        let limit = view["limits"]["memory"].as_u64().expect("limit") as usize;
        let (code, Json(refused)) = memory_write(Json(json!({
            "store": "memory",
            "entries": ["x".repeat(limit + 1)],
        })))
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{refused}");
        assert!(
            refused["error"].as_str().unwrap().contains("budget"),
            "{refused}"
        );
    }

    /// The Files tab's whole contract, through the handlers: list, open
    /// (with the sha to save against), save, a save against a stale sha
    /// refused as 409 without overwriting, rename, history, diff,
    /// delete, restore.
    /// The bodies the desktop's api.tcl builds for multi answers — byte
    /// for byte, captured from `::vapi::ask_decide_answers` and
    /// `::vapi::ask_decide_multi` — decide the ask they name. asktest
    /// checks the Tcl half at its wire (it stubs `::vapi::post` and
    /// reads the body); this is the other half, so the hand-built Tcl
    /// JSON and `ask::answers_value` cannot drift apart unnoticed.
    #[tokio::test]
    async fn the_desktops_multi_answer_bodies_decide_the_ask() {
        use veles_core::ask::{AskChoice, AskKind, AskQuestion, AskRequest};
        let mq = || {
            AskRequest::multi_question(
                "s-wire",
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
        };
        async fn decide(req: &AskRequest, body: &str) -> veles_core::ask::AskAnswer {
            let leg = veles_core::ask::register(req, None, None).expect("registers");
            let parsed: DecideRequest = serde_json::from_str(body).expect("the body parses");
            let Json(v) = decide_ask(Path(req.id.clone()), Json(parsed)).await;
            assert_eq!(v["ok"], true, "{v}");
            leg.wait().await
        }

        let a = decide(
            &mq(),
            r#"{"answers":{"q1":{"selected":["c1"],"text":""},"q2":{"selected":["c1","c2"],"text":""},"q3":{"selected":[],"text":"Imperial \"Grid\""}}}"#,
        )
        .await;
        assert_eq!(a.per_question["q1"].selected, vec!["c1"]);
        assert_eq!(a.per_question["q2"].selected, vec!["c1", "c2"]);
        assert_eq!(
            a.per_question["q3"].free_text.as_deref(),
            Some("Imperial \"Grid\"")
        );

        let a = decide(&mq(), r#"{"answers":{"q2":{"selected":["c2"],"text":""}}}"#).await;
        assert_eq!(a.per_question.len(), 1, "{a:?}");
        assert_eq!(a.per_question["q2"].selected, vec!["c2"]);

        let single = AskRequest::new(
            "s-wire",
            AskKind::Clarify,
            "pick any",
            vec![AskChoice::new("c1", "a"), AskChoice::new("c2", "b")],
        );
        let a = decide(&single, r#"{"answers":["c1","c2"]}"#).await;
        assert_eq!(a.selected, vec!["c1", "c2"]);
    }

    #[tokio::test]
    async fn memory_files_round_trip_through_the_routes() {
        let (_d, _srv, _home, _env) = scratch_server();
        let q = |pairs: &[(&str, &str)]| -> Query<StdMap<String, String>> {
            Query(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        };

        // New file: base_sha null.
        let (code, Json(b)) = memory_file_put(Json(json!({
            "path": "reference/t.md", "content": "one\n", "base_sha": null,
        })))
        .await;
        assert_eq!(code, StatusCode::OK, "{b}");
        // base_sha is required, not defaulted.
        let (code, _) =
            memory_file_put(Json(json!({ "path": "reference/t.md", "content": "x" }))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);

        let (_, Json(files)) = memory_files().await;
        assert!(files["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "reference/t.md"));

        let (code, Json(opened)) = memory_file_get(q(&[("path", "reference/t.md")])).await;
        assert_eq!(code, StatusCode::OK, "{opened}");
        let sha = opened["sha256"].as_str().unwrap().to_string();

        // The agent writes while the panel has it open.
        veles_core::memory::MemoryStore::new()
            .write_file("reference/t.md", "agent\n")
            .unwrap();
        let (code, Json(b)) = memory_file_put(Json(json!({
            "path": "reference/t.md", "content": "mine\n", "base_sha": sha,
        })))
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{b}");
        assert!(b["current_sha"].is_string(), "{b}");
        let (_, Json(now)) = memory_file_get(q(&[("path", "reference/t.md")])).await;
        assert_eq!(now["content"], "agent\n", "nothing overwritten");

        // Save against the current sha.
        let (code, _) = memory_file_put(Json(json!({
            "path": "reference/t.md", "content": "mine\n", "base_sha": now["sha256"],
        })))
        .await;
        assert_eq!(code, StatusCode::OK);

        let (code, Json(d)) = memory_diff(q(&[("path", "reference/t.md")])).await;
        assert_eq!(code, StatusCode::OK, "{d}");
        assert!(d["diff"].as_str().unwrap().contains("-agent\n+mine"), "{d}");

        let (code, _) = memory_file_move(Json(json!({
            "from": "reference/t.md", "to": "projects/t.md",
        })))
        .await;
        assert_eq!(code, StatusCode::OK);
        let (code, _) = memory_file_get(q(&[("path", "reference/t.md")])).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        let (_, Json(log)) = memory_log(q(&[("path", "projects/t.md")])).await;
        let first = log["rows"][0]["rev"].as_i64().unwrap();
        let (code, _) = memory_file_delete(Json(json!({ "path": "projects/t.md" }))).await;
        assert_eq!(code, StatusCode::OK);
        let (code, _) = memory_file_get(q(&[("path", "projects/t.md")])).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) =
            memory_restore(Json(json!({ "path": "projects/t.md", "rev": first }))).await;
        assert_eq!(code, StatusCode::OK);
        let (_, Json(back)) = memory_file_get(q(&[("path", "projects/t.md")])).await;
        assert_eq!(back["content"], "mine\n");
    }

    /// A preset session is the point of the seeding: it opens with its
    /// standing context already in the transcript, as a `system`
    /// message rather than a fake question from the user.
    #[tokio::test]
    async fn a_seeded_session_opens_with_its_preamble() {
        let (_d, srv, _home, _env) = scratch_server();
        let (code, Json(created)) = new_session(
            State(srv.clone()),
            Some(Json(json!({
                "title": "Dev: live scripts",
                "preamble": "You are working on veles-desktop's own Tcl.",
            }))),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{created}");
        let sid = created["session_id"].as_str().expect("id").to_string();

        let (_, Json(history)) = history(State(srv.clone()), Path(sid.clone())).await;
        let msgs = history["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 1, "{history}");
        assert_eq!(msgs[0]["role"], json!("system"), "{history}");
        assert!(msgs[0]["content"]
            .as_str()
            .unwrap()
            .contains("veles-desktop's own Tcl"));

        let (_, Json(list)) = list_sessions(State(srv), axum::extract::Query(HashMap::new())).await;
        let row = list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == json!(sid))
            .expect("listed");
        assert_eq!(row["title"], json!("Dev: live scripts"), "{list}");
    }

    #[tokio::test]
    async fn desktop_join_http_uses_the_target_core_and_never_starts_an_ordinary_turn() {
        use tower::ServiceExt;
        let (_dir, srv, _home, env) = scratch_server();
        let core = crate::gateway::GatewayCore::new_join_test(srv.store.clone());
        let sid = core
            .test_sender(
                "irc",
                "#chan",
                "irc:alice",
                veles_core::acl::SenderContext {
                    mask: Some("alice!u@example.invalid".into()),
                    network: Some("testnet".into()),
                    channel: Some("#chan".into()),
                    platform: Some("irc".into()),
                    chat_id: Some("#chan".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        crate::gateway::set_global_core(core.clone());
        let app = router(srv, Arc::new(token_cfg("join-test")));
        let call = |app: Router, path: String, body: Value| async move {
            let req = axum::http::Request::post(&path)
                .header("authorization", "Bearer join-test")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap();
            let response = app.oneshot(req).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        };
        let (status, body) = call(
            app.clone(),
            "/api/join".into(),
            json!({"session_id": sid, "instance": "laptop", "owner_session": "mine"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["join"]["join_id"].as_str().unwrap().to_string();
        assert_eq!(body["join"]["from_instance"], "laptop");
        assert_eq!(body["join"]["from_session"], "mine");
        let raw = core
            .store
            .lock_ok()
            .meta_get(&format!("join:gateway:v1:desktop:join:{id}"))
            .unwrap();
        assert!(!raw.contains("from_instance") && !raw.contains("from_session"));

        let (status, body) = call(
            app.clone(),
            "/api/chat".into(),
            json!({"session_id": sid, "join_id": id, "text": "owner through HTTP", "attachments": []}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["join"], true);
        let (mail_kind, mail_body): (String, String) = core
            .store
            .lock_ok()
            .conn()
            .query_row(
                "SELECT kind, body_sha FROM session_mail WHERE to_session = ?1 ORDER BY created_at DESC LIMIT 1",
                [&sid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(mail_kind, veles_core::mail::KIND_JOIN);
        assert!(!mail_body.is_empty());
        assert!(core.store.lock_ok().session_exists(&sid).unwrap());
        let (status, _) = call(
            app.clone(),
            "/api/chat".into(),
            json!({"session_id": sid, "join_id": id, "text": "/steer not allowed here", "attachments": []}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let mail_count: i64 = core
            .store
            .lock_ok()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session_mail WHERE to_session = ?1",
                [&sid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mail_count, 1, "rejected joined /steer adds no turn");
        let (status, body) = call(
            app,
            "/api/join/leave".into(),
            json!({"session_id": sid, "join_id": id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert!(core
            .store
            .lock_ok()
            .meta_get(&format!("join:gateway:v1:desktop:join:{id}"))
            .is_none());
        drop(env);
    }

    #[tokio::test]
    async fn desktop_join_status_revalidates_the_exact_saved_binding() {
        use tower::ServiceExt;
        let (_dir, srv, _home, _env) = scratch_server();
        let core = crate::gateway::GatewayCore::new_join_test(srv.store.clone());
        let sid = core
            .test_sender(
                "irc",
                "#chan",
                "irc:alice",
                veles_core::acl::SenderContext {
                    mask: Some("alice!u@example.invalid".into()),
                    network: Some("testnet".into()),
                    channel: Some("#chan".into()),
                    platform: Some("irc".into()),
                    chat_id: Some("#chan".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let binding = core.desktop_join("desktop:join:j-status", &sid).unwrap();
        crate::gateway::set_global_core(core.clone());
        let app = router(srv, Arc::new(token_cfg("join-test")));
        let get = |app: Router, path: String| async move {
            let req = axum::http::Request::get(&path)
                .header("authorization", "Bearer join-test")
                .body(axum::body::Body::empty())
                .unwrap();
            let response = app.oneshot(req).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        };
        let path = format!(
            "/api/join/status?join_id={}&session_id={sid}",
            binding.join_id
        );
        let (status, body) = get(app.clone(), path.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["join"]["join_id"], binding.join_id);
        assert_eq!(body["join"]["target_session"], sid);
        assert_eq!(body["join"]["from_instance"], "");
        let revoked = veles_core::acl::Principal {
            subject: "owner".into(),
            role: veles_core::acl::Role::Owner,
            ..(*core
                .sessions
                .lock_ok()
                .values()
                .next()
                .unwrap()
                .standing_principal()
                .unwrap())
            .clone()
        };
        core.sessions
            .lock_ok()
            .values()
            .next()
            .unwrap()
            .set_principal(revoked);
        let (status, body) = get(app.clone(), path.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["error"].as_str().unwrap().contains("no longer valid"));
        assert!(core.desktop_leave(&binding.join_id, &sid).is_some());
        let (status, _) = get(app, path).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    /// axum panics on a route conflict when the Router is BUILT, and the
    /// table used to be built only inside `serve` — so the first time
    /// anyone learned about a bad route was the gateway failing to
    /// start. Building it here turns that into a test failure.
    #[tokio::test]
    async fn editor_save_refuses_a_stale_buffer_without_overwriting_the_file() {
        use tower::ServiceExt;
        let (_d, srv, _h, _env) = scratch_server();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buffer.txt");
        std::fs::write(&path, "original").unwrap();
        let Json(read) = crate::panes::fs_file(
            State(srv.clone()),
            Query(crate::panes::FileQuery {
                path: path.to_string_lossy().into_owned(),
            }),
        )
        .await;
        let base = read["sha256"]
            .as_str()
            .expect("the read supplies a base")
            .to_string();
        std::fs::write(&path, "changed by another editor").unwrap();
        let app = router(srv, Arc::new(token_cfg("editor-test")));
        let call = |body: Value| {
            let app = app.clone();
            async move {
                let req = axum::http::Request::post("/api/fs/write")
                    .header("authorization", "Bearer editor-test")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                let bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap()
            }
        };
        let reply = call(json!({"path": path, "content": "my stale edit", "base_sha": base})).await;
        assert_eq!(reply["conflict"], true, "{reply}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "changed by another editor"
        );
        let reply = call(json!({"path": path, "content": "unconditional clobber"})).await;
        assert_eq!(reply["conflict"], true, "{reply}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "changed by another editor"
        );
    }

    #[test]
    fn router_builds() {
        let (_d, srv, _home, _env) = scratch_server();
        let auth = Arc::new(veles_core::authn::from_env(
            "DESKTOP_PEER_TOKENS_UNSET_FOR_TEST",
            "DESKTOP_BEARER_TOKEN_UNSET_FOR_TEST",
        ));
        let _ = router(srv, auth);
    }

    /// Drive the REAL router, because the 405 was a ROUTING bug, not a
    /// handler bug: `session_mechanism` had implemented the read all
    /// along and the route simply never offered GET. A handler-level
    /// test cannot see that, and neither could `router_builds` — only
    /// asking the router for the method does.
    #[tokio::test]
    async fn the_router_offers_get_on_the_mechanism_route() {
        use tower::ServiceExt;
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "mech-routing";
        srv.store.lock_ok().create_session(sid).expect("create");
        let auth = Arc::new(veles_core::authn::from_env(
            "DESKTOP_PEER_TOKENS_UNSET_FOR_TEST",
            "DESKTOP_BEARER_TOKEN_UNSET_FOR_TEST",
        ));
        let app = router(srv, auth).into_make_service_with_connect_info::<SocketAddr>();
        // The auth middleware needs a peer address; go through
        // make_service the same way `serve` does.
        let svc = <_ as tower::Service<SocketAddr>>::call(
            &mut { app },
            "127.0.0.1:9999".parse::<SocketAddr>().unwrap(),
        )
        .await
        .expect("connect-info service");

        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/api/sessions/{sid}/mechanism"))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.expect("response");
        assert_ne!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "GET must be routed — this is the exact 405 the overlay hit"
        );
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The bug the user reported: the Mechanisms overlay reads this with
    /// a GET and got `405 Method Not Allowed / allow: POST`, so the pane
    /// showed "mechanisms unreachable" instead of a single row.
    #[tokio::test]
    async fn mechanism_view_answers_a_get_with_the_catalogue() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "mech-get";
        srv.store.lock_ok().create_session(sid).expect("create");

        let (code, Json(body)) =
            session_mechanism_view(State(srv.clone()), Path(sid.to_string())).await;
        assert_eq!(code, StatusCode::OK);
        let mechs = body["mechanisms"].as_array().expect("catalogue");
        assert_eq!(
            mechs.len(),
            veles_core::notes::catalogue().len(),
            "every mechanism is listed, fired this session or not"
        );
        assert!(mechs
            .iter()
            .all(|m| m["key"].is_string() && m["config_key"].is_string() && m["help"].is_string()));
        assert!(body["overrides"].as_array().expect("overrides").is_empty());
    }

    /// The GET and the POST's no-name branch must not drift: the slash
    /// command reads through one and the overlay through the other.
    #[tokio::test]
    async fn mechanism_get_and_post_read_agree_after_an_override() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "mech-agree";
        srv.store.lock_ok().create_session(sid).expect("create");

        let key = veles_core::notes::catalogue()[0].kind.as_str().to_string();
        let (code, _) = session_mechanism(
            State(srv.clone()),
            Path(sid.to_string()),
            Json(json!({ "name": key, "value": "off" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "the override must apply");

        let (_, Json(via_get)) =
            session_mechanism_view(State(srv.clone()), Path(sid.to_string())).await;
        let (_, Json(via_post)) = session_mechanism(
            State(srv.clone()),
            Path(sid.to_string()),
            Json(json!({ "name": "", "value": "" })),
        )
        .await;
        assert_eq!(via_get, via_post, "one snapshot, two doors");
        let ovr = via_get["overrides"].as_array().unwrap();
        assert_eq!(ovr.len(), 1, "{via_get}");
        assert_eq!(ovr[0]["key"], json!(key));
        assert_eq!(ovr[0]["value"], json!("off"));
    }

    /// config_set writes through configedit, so the user's comments and
    /// unrelated keys survive — and the response says what the change
    /// actually did to the running gateway.
    #[tokio::test]
    async fn config_set_edits_in_place_and_reports_its_effect() {
        let (_d, _srv, home, _env) = scratch_server();
        let path = home.path().join("config.toml");
        std::fs::write(
            &path,
            "# a comment worth keeping\n[agent]\nuser_agent = \"mine\"\n",
        )
        .expect("seed config");

        let (code, Json(body)) = config_set(
            State(_srv.clone()),
            Json(json!({
                "path": "agent.guards.max_iterations",
                "value": 42,
            })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["effect"], json!("new_sessions"), "{body}");
        assert!(body["backup"].is_string(), "the old file is kept: {body}");

        let after = std::fs::read_to_string(&path).expect("read back");
        assert!(after.contains("# a comment worth keeping"), "{after}");
        assert!(after.contains("user_agent = \"mine\""), "{after}");
        let parsed: VelesConfig = toml::from_str(&after).expect("{after}");
        assert_eq!(parsed.agent.guards.max_iterations, 42);
    }

    /// A form field nothing typed sends its text as `raw`: the TOML
    /// library reads `7` as a number and a phrase as a string, and the
    /// form route shows the written value back, typed.
    #[tokio::test]
    async fn config_set_reads_raw_text_as_a_toml_literal_and_the_form_shows_it() {
        let (_d, srv, home, _env) = scratch_server();
        let path = home.path().join("config.toml");
        std::fs::write(&path, "# mine\n[agent]\n").expect("seed config");

        let (code, Json(body)) = config_set(
            State(srv.clone()),
            Json(json!({ "path": "agent.guards.max_iterations", "raw": "7" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let (code, Json(body)) = config_set(
            State(srv.clone()),
            Json(json!({ "path": "agent.user_agent", "raw": "my agent" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let after = std::fs::read_to_string(&path).expect("read back");
        let parsed: VelesConfig = toml::from_str(&after).expect("{after}");
        assert_eq!(
            parsed.agent.guards.max_iterations, 7,
            "a number stays a number"
        );
        assert!(after.contains("user_agent = \"my agent\""), "{after}");
        assert!(after.contains("# mine"), "{after}");

        let Json(form) = config_form().await;
        let field = form["sections"]
            .as_array()
            .expect("sections")
            .iter()
            .flat_map(|s| s["knobs"].as_array().expect("knobs").iter())
            .find(|k| k["path"] == "agent.guards.max_iterations")
            .expect("the option is on the form")
            .clone();
        assert_eq!(field["kind"], "int", "{field}");
        assert_eq!(field["value"], json!(7), "{field}");
    }

    /// The apps routes (veles_core::apps): the list, an app's files to
    /// run it, and its store — which the gateway refuses to an app that
    /// did not declare `store`, whatever the window allowed.
    #[tokio::test]
    async fn app_routes_list_read_and_keep_only_what_was_declared() {
        let (_d, _srv, _home, _env) = scratch_server();
        veles_core::apps::create("eggs", "Eggs", &["store".into()]).expect("eggs");
        veles_core::apps::create("plain", "Plain", &[]).expect("plain");

        let Json(list) = apps_list().await;
        let names: Vec<&str> = list["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["eggs", "plain"]);

        let (code, Json(src)) = app_source(Path("eggs".into())).await;
        assert_eq!(code, StatusCode::OK, "{src}");
        assert!(src["files"]["main.tcl"]
            .as_str()
            .unwrap()
            .contains("ttk::label"));
        assert_eq!(src["info"]["capabilities"], json!(["store"]));
        let (code, _) = app_source(Path("../etc".into())).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        let (code, Json(r)) = app_store(
            Path("eggs".into()),
            Json(json!({"op": "set", "key": "n", "value": "12"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{r}");
        let (_, Json(r)) =
            app_store(Path("eggs".into()), Json(json!({"op": "get", "key": "n"}))).await;
        assert_eq!(r["value"], json!("12"));
        let (code, Json(r)) = app_store(
            Path("plain".into()),
            Json(json!({"op": "set", "key": "n", "value": "1"})),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "undeclared: {r}");
    }

    // ── the setup wizard's three routes ─────────────────────────────
    //
    // The pure half of the contract (block shapes, edit-in-place, 0600)
    // is in `veles_core::setup::tests`; these pin the SEAM: state reads the
    // FILE, preview is pure, apply edits + reloads the live handle, a
    // stale preview is refused, and a bad answer is refused by name.

    /// The body lib/setup.tcl's `answers_body` builds, byte for byte —
    /// setuptest.tcl pins the Tcl side to this same string, so the two
    /// halves of the wire are checked against one text.
    const TK_SETUP_BODY: &str = r#"{"provider": "openai_compatible", "base_url": "http://127.0.0.1:11434/v1", "key_env": "OLLAMA_KEY", "api_mode": "chat_completions", "provider_name": "ollama", "model": "llama3", "default_provider": "ollama", "telegram": true, "irc_server": "", "irc_port": 6697, "irc_use_tls": true, "irc_nickname": "", "irc_channels": "", "irc_sasl_username": "", "acl": "legacy_open", "acl_owner": "", "locale": "de", "skipped": ["irc"]}"#;

    fn preview_body(v: Value) -> SetupPreviewBody {
        serde_json::from_value(v).expect("setup preview body")
    }

    fn apply_body(v: Value) -> SetupApplyBody {
        serde_json::from_value(v).expect("apply body")
    }

    /// A secret value in the apply body from another machine is refused
    /// before anything is written; from this machine it goes into the
    /// vault, the response names it and never carries it, and `state`
    /// reports it stored.
    #[tokio::test]
    async fn setup_models_lists_with_the_typed_key_and_refuses_it_from_afar() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("authorization:") {
                        log.lock().unwrap().push(v.trim().to_string());
                    }
                    line.clear();
                }
                let body = r#"{"data":[{"id":"b-model"},{"id":"a-model"}]}"#;
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let body = || SetupModelsBody {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            key_env: "VELES_T_MODELS_KEY".into(),
            key: "sk-models-typed-0042".into(),
        };
        let remote: SocketAddr = "10.0.30.3:4444".parse().unwrap();
        let (code, Json(r)) = setup_models(Some(ConnectInfo(remote)), Json(body())).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{r}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "the key crossed to the provider anyway"
        );
        let local: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let (code, Json(r)) = setup_models(Some(ConnectInfo(local)), Json(body())).await;
        assert_eq!(code, StatusCode::OK, "{r}");
        assert_eq!(r["models"], json!(["a-model", "b-model"]), "{r}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["bearer sk-models-typed-0042"]
        );
    }

    #[tokio::test]
    async fn setup_state_offers_the_provider_presets() {
        let (_d, srv, _home, _env) = scratch_server();
        let Json(st) = setup_state(State(srv), None).await;
        let ids: Vec<&str> = st["presets"]
            .as_array()
            .expect("presets")
            .iter()
            .filter_map(|p| p["choice"].as_str())
            .collect();
        assert!(
            ids.contains(&"openrouter") && ids.contains(&"ollama"),
            "{st}"
        );
        // The desktop's provider step copies api_mode from the row it
        // shows; the engine fills the row's driver in. The Anthropic row
        // is the native wire (0.1.2), not the OpenAI-compatible layer.
        let anthropic = st["presets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["choice"] == "anthropic")
            .expect("an anthropic row");
        assert_eq!(anthropic["api_mode"], "plugin", "{anthropic}");
        assert_eq!(anthropic["driver"], "anthropic-messages", "{anthropic}");
        assert_eq!(
            anthropic["base_url"], "https://api.anthropic.com",
            "{anthropic}"
        );
    }

    #[tokio::test]
    async fn setup_apply_takes_secret_values_only_from_this_machine() {
        let _en = veles_core::i18n::ScopeGuard::enter(Some("en".into()));
        let (_d, srv, home, _env) = scratch_server();
        let value = "sk-typed-into-the-wizard-00111";
        let body = || {
            let mut b = apply_body(json!({
                "provider": "openai_compatible",
                "base_url": "https://api.example.com/v1",
                "key_env": "VELES_T_SETUP_KEY",
                "provider_name": "ex",
                "model": "m",
            }));
            b.secrets.insert("VELES_T_SETUP_KEY".into(), value.into());
            b
        };
        let remote: SocketAddr = "10.0.30.3:4444".parse().unwrap();
        let (code, Json(r)) =
            setup_apply(State(srv.clone()), Some(ConnectInfo(remote)), Json(body())).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{r}");
        assert!(
            !home.path().join("config.toml").exists(),
            "written despite the refusal"
        );
        let local: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let (code, Json(r)) =
            setup_apply(State(srv.clone()), Some(ConnectInfo(local)), Json(body())).await;
        assert_eq!(code, StatusCode::OK, "{r}");
        assert!(!r.to_string().contains(value), "{r}");
        assert_eq!(r["secrets_stored"], json!(["VELES_T_SETUP_KEY"]), "{r}");
        let Json(st) = setup_state(State(srv.clone()), Some(ConnectInfo(local))).await;
        assert_eq!(st["stored"], json!(["VELES_T_SETUP_KEY"]), "{st}");
        assert_eq!(st["accepts_secrets"], json!(true), "{st}");
        let Json(st) = setup_state(State(srv), Some(ConnectInfo(remote))).await;
        assert_eq!(st["accepts_secrets"], json!(false), "{st}");
        assert!(!st.to_string().contains(value), "{st}");
    }

    #[tokio::test]
    async fn setup_state_sees_a_first_run_and_then_the_file() {
        let (_d, srv, home, _env) = scratch_server();

        let Json(first) = setup_state(State(srv.clone()), None).await;
        assert_eq!(first["first_run"], json!(true), "{first}");
        assert_eq!(first["current"], Value::Null, "nothing to prefill: {first}");

        std::fs::write(
            home.path().join("config.toml"),
            "[model]\nprovider = \"p\"\ndefault = \"m\"\n\n[[custom_providers]]\nname = \"p\"\nbase_url = \"http://x/v1\"\nkey_env = \"P_KEY\"\n",
        )
        .expect("seed");
        let Json(second) = setup_state(State(srv.clone()), None).await;
        // `first_run` is ONLY "config.toml is absent".
        assert_eq!(second["first_run"], json!(false), "{second}");
        // The prefill is the FILE, not the live handle (which still holds
        // scratch_server's test-provider/test-model): an edit starts from
        // the file, so that is what the wizard must show.
        assert_eq!(second["current"]["model"], json!("m"), "{second}");
        assert_eq!(second["current"]["provider_name"], json!("p"), "{second}");
        assert_eq!(second["current"]["key_env"], json!("P_KEY"), "{second}");
    }

    #[tokio::test]
    async fn setup_preview_is_pure_and_carries_names_not_values() {
        let (_d, _srv, home, _env) = scratch_server();
        let (code, Json(body)) = setup_preview(Json(preview_body(json!({
            "provider": "openai_compatible",
            "base_url": "http://127.0.0.1:11434/v1",
            "key_env": "OLLAMA_KEY",
            "model": "llama3",
            "provider_name": "ollama",
        }))))
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["mode"], json!("fresh"), "{body}");
        assert_eq!(body["base_sig"], json!(""), "no file, no base: {body}");
        let cfg = body["config_toml"].as_str().expect("config text");
        assert!(
            cfg.contains("[model]") && cfg.contains("OLLAMA_KEY"),
            "{cfg}"
        );
        assert!(
            !home.path().join("config.toml").exists(),
            "preview must not touch disk"
        );
        assert!(!home.path().join(".env").exists());
        assert_eq!(body["secrets"], json!(["OLLAMA_KEY"]), "{body}");
    }

    #[tokio::test]
    async fn setup_preview_refuses_by_name_and_never_echoes_a_key() {
        let (_d, _srv, _home, _env) = scratch_server();
        let (code, Json(body)) = setup_preview(Json(preview_body(json!({
            "provider": "openai_compatible",
            "api_mode": "smoke_signals",
        }))))
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("smoke_signals"),
            "{body}"
        );

        // A key pasted into the NAME field: refused, and not repeated
        // back in a response a log or a screenshot might keep.
        let (code, Json(body)) = setup_preview(Json(preview_body(json!({
            "provider": "openai_compatible",
            "key_env": "sk-proj-abcdef0123456789",
        }))))
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            !body.to_string().contains("sk-proj-abcdef0123456789"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn setup_takes_the_body_the_desktop_builds() {
        let (_d, srv, home, _env) = scratch_server();
        let a: veles_core::setup::SetupAnswers =
            serde_json::from_str(TK_SETUP_BODY).expect("the Tk body deserializes");
        assert_eq!(a.skipped, vec!["irc".to_string()]);
        assert_eq!(a.locale, "de");
        let (code, Json(pv)) = setup_preview(Json(
            serde_json::from_value(serde_json::to_value(a).unwrap()).unwrap(),
        ))
        .await;
        assert_eq!(code, StatusCode::OK, "{pv}");

        let cfg = pv["config_toml"].as_str().unwrap();
        assert!(
            cfg.contains("locale = \"de\"") && !cfg.contains("[platforms.irc]"),
            "{cfg}"
        );

        // Write the same body with `locale` blanked: apply reloads the live
        // config, and reload_config re-inits the PROCESS locale when
        // tui.locale changes — writing "de" here switched the whole test
        // binary to German and reddened every English assertion running
        // beside it (six, in the first gate run).
        let mut body: Value = serde_json::from_str(TK_SETUP_BODY).unwrap();
        body["locale"] = json!("");
        let (_, Json(pv2)) = setup_preview(Json(preview_body(body.clone()))).await;
        body["expect_base"] = pv2["base_sig"].clone();
        let (code, Json(r)) = setup_apply(State(srv), None, Json(apply_body(body))).await;
        assert_eq!(code, StatusCode::OK, "{r}");
        let written = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert_eq!(
            written,
            pv2["config_toml"].as_str().unwrap(),
            "the preview IS the write"
        );
    }

    #[tokio::test]
    async fn a_secret_is_stored_only_from_a_trusted_path_and_never_echoed() {
        let (_d, _srv, _home, _env) = scratch_server();
        let value = "sk-typed-into-the-masked-field-01";
        let body = |name: &str| {
            Json(SecretSetBody {
                name: name.into(),
                value: value.into(),
                ask_id: None,
            })
        };
        let remote: SocketAddr = "10.0.30.3:5555".parse().unwrap();
        let (code, Json(out)) =
            secrets_set(Some(ConnectInfo(remote)), body("VELES_T_DESK_KEY")).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{out}");
        assert!(veles_core::secrets::resolve("VELES_T_DESK_KEY").is_none());
        let local: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let (code, Json(out)) =
            secrets_set(Some(ConnectInfo(local)), body("VELES_T_DESK_KEY")).await;
        assert_eq!(code, StatusCode::OK, "{out}");
        assert!(!out.to_string().contains(value), "{out}");
        assert_eq!(
            veles_core::secrets::resolve("VELES_T_DESK_KEY").as_deref(),
            Some(value)
        );
        let Json(list) = secrets_list(None).await;
        assert!(list.to_string().contains("VELES_T_DESK_KEY"), "{list}");
        assert!(!list.to_string().contains(value), "{list}");
        // A network the operator lists — the WireGuard subnet a laptop's
        // desktop reaches this gateway over — may send values too; the
        // live config is read, so no restart. Its neighbour may not.
        let Json(l) = secrets_list(Some(ConnectInfo(remote))).await;
        assert_eq!(l["accepts_values"], json!(false), "{l}");
        std::fs::write(
            _home.path().join("config.toml"),
            "[vault]\ntrusted_networks = [\"10.0.30.0/24\"]\n",
        )
        .unwrap();
        let Json(l) = secrets_list(Some(ConnectInfo(remote))).await;
        assert_eq!(l["accepts_values"], json!(true), "{l}");
        let (code, Json(out)) =
            secrets_set(Some(ConnectInfo(remote)), body("VELES_T_WG_KEY")).await;
        assert_eq!(code, StatusCode::OK, "{out}");
        let outside: SocketAddr = "10.0.31.3:5555".parse().unwrap();
        let (code, _) = secrets_set(Some(ConnectInfo(outside)), body("VELES_T_LAN_KEY")).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(veles_core::secrets::resolve("VELES_T_LAN_KEY").is_none());
        // An ask id that is not open is refused, not silently stored.
        let (code, _) = secrets_set(
            None,
            Json(SecretSetBody {
                name: "VELES_T_OTHER".into(),
                value: value.into(),
                ask_id: Some("ask-that-never-was".into()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn setup_apply_edits_in_place_and_reloads_the_live_handle() {
        // The notes are translated; pin this thread to the source language
        // so a test elsewhere switching the process locale cannot race it.
        let _en = veles_core::i18n::ScopeGuard::enter(Some("en".into()));
        let (_d, srv, home, _env) = scratch_server();
        let cfg_path = home.path().join("config.toml");
        let before = "# keep me\n[agent.guards]\nmax_iterations = 42\n";
        std::fs::write(&cfg_path, before).expect("seed");

        let (code, Json(body)) = setup_apply(
            State(srv.clone()),
            None,
            Json(apply_body(json!({
                "provider": "ali_token_plan",
                "telegram": true,
                "irc_server": "irc.libera.chat",
                "irc_channels": "#veles",
                "acl": "curated",
                "acl_owner": "telegram:123456",
            }))),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["mode"], json!("edit"), "{body}");
        assert_eq!(body["reloaded"], json!(true), "{body}");
        assert_eq!(body["env_written"], json!(true), "{body}");

        let bak = body["config_backup"].as_str().expect("backup named");
        assert_eq!(std::fs::read_to_string(bak).expect("backup"), before);
        let written = std::fs::read_to_string(&cfg_path).expect("config");
        // EDITED, not replaced: the operator's section and comment stay.
        assert!(
            written.contains("# keep me") && written.contains("max_iterations = 42"),
            "{written}"
        );
        assert!(
            written.contains("owner = [\"telegram:123456\"]"),
            "{written}"
        );
        let notes = body["notes"].as_array().expect("notes");
        assert!(
            notes
                .iter()
                .any(|n| n.as_str().unwrap().contains("CURATED")),
            "{body}"
        );

        // R4.5: the LIVE config the gateway runs on is the new one.
        let live = srv.cfg();
        assert_eq!(live.model.provider.as_deref(), Some("ali-token-plan"));
        assert_eq!(live.acl.owner, vec!["telegram:123456".to_string()]);
        assert_eq!(live.agent.guards.max_iterations, 42);
    }

    #[tokio::test]
    async fn setup_apply_refuses_a_stale_preview_and_writes_nothing() {
        let (_d, srv, home, _env) = scratch_server();
        let cfg_path = home.path().join("config.toml");
        std::fs::write(&cfg_path, "[agent.guards]\nmax_iterations = 42\n").expect("seed");
        let (_, Json(pv)) = setup_preview(Json(preview_body(json!({})))).await;
        // Something else edits the file after the preview was read.
        let edited = "[agent.guards]\nmax_iterations = 7\n";
        std::fs::write(&cfg_path, edited).expect("edit");

        let (code, Json(body)) = setup_apply(
            State(srv),
            None,
            Json(apply_body(json!({ "expect_base": pv["base_sig"] }))),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{body}");
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            edited,
            "nothing written"
        );
    }

    /// An edit that would leave a config the agent cannot read is
    /// refused, and the file on disk is untouched.
    #[tokio::test]
    async fn config_set_refuses_an_edit_that_would_break_the_file() {
        let (_d, _srv, home, _env) = scratch_server();
        let path = home.path().join("config.toml");
        let before = "[agent]\nuser_agent = \"mine\"\n";
        std::fs::write(&path, before).expect("seed config");

        let (code, Json(body)) = config_set(
            State(_srv.clone()),
            Json(json!({
                "path": "agent.guards.max_iterations",
                "value": "not a number",
            })),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a refused edit writes nothing"
        );
    }

    /// `set: false` and "set to the default" are different answers, and
    /// a three-state control needs to tell them apart.
    #[tokio::test]
    async fn config_get_distinguishes_unset_from_set() {
        let (_d, _srv, home, _env) = scratch_server();
        let path = home.path().join("config.toml");
        std::fs::write(&path, "[agent]\n").expect("seed");

        let (_, Json(before)) = config_get(Query(ConfigPathQuery {
            path: "agent.guards.verification_nudge".into(),
        }))
        .await;
        assert_eq!(before["set"], json!(false), "{before}");

        let (_, Json(_)) = config_set(
            State(_srv.clone()),
            Json(json!({
                "path": "agent.guards.verification_nudge",
                "value": true,
            })),
        )
        .await;
        let (_, Json(after)) = config_get(Query(ConfigPathQuery {
            path: "agent.guards.verification_nudge".into(),
        }))
        .await;
        assert_eq!(after["set"], json!(true), "{after}");
        assert_eq!(after["value"], json!(true), "{after}");
    }

    /// A disabled plugin has to stay VISIBLE, or there is no row left to
    /// switch it back on with. This is the bug that made the plugins
    /// overlay unfixable as a manager: it listed the filtered set.
    #[tokio::test]
    async fn plugins_list_still_shows_a_disabled_plugin() {
        let (_d, srv, home, _env) = scratch_server();
        let dir = home.path().join("plugins").join("noisy");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("plugin.toml"),
            "name = \"noisy\"\nkind = \"tcl\"\nentry = \"plugin.tcl\"\n",
        )
        .expect("manifest");
        std::fs::write(dir.join("plugin.tcl"), "# stub\n").expect("entry");
        std::fs::write(
            home.path().join("config.toml"),
            "[plugins]\ndisabled = [\"noisy\"]\n",
        )
        .expect("config");

        let Json(body) = plugins_list(State(srv)).await;
        let plugins = body["plugins"].as_array().expect("plugins");
        let noisy = plugins
            .iter()
            .find(|p| p["name"] == json!("noisy"))
            .unwrap_or_else(|| panic!("a disabled plugin must still be listed: {body}"));
        assert_eq!(noisy["enabled"], json!(false), "{body}");
        assert_eq!(noisy["explicitly_disabled"], json!(true), "{body}");
        assert_eq!(body["allowlist_active"], json!(false), "{body}");
    }

    /// The process-plugin controls check the verb before anything else,
    /// and a process with no plugin host says so instead of a 404 that
    /// reads like "no such plugin".
    #[tokio::test]
    async fn process_plugin_controls_answer_without_a_host() {
        let (code, Json(body)) = process_plugin_control_with(None, "echo", "explode").await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"].as_str().unwrap_or("").contains("explode"),
            "{body}"
        );
        let (code, Json(body)) = process_plugin_control_with(None, "echo", "reload").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let Json(body) = process_plugins_status_with(None);
        assert_eq!(body["host"], json!(false), "{body}");
        assert_eq!(body["instances"], json!([]), "{body}");
    }

    /// The desktop must never invent sessions: asking for an agent on an
    /// unknown id is an error, not an implicit create.
    #[test]
    fn agent_for_refuses_unknown_sessions() {
        let (_d, srv, _home, _env) = scratch_server();
        // Agent has no Debug impl, so match rather than expect_err.
        let err = match srv.agent_for("nope") {
            Ok(_) => panic!("unknown session must fail, not create an agent"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("session not found"), "got: {err}");
        assert!(
            !srv.store.lock_ok().session_exists("nope").unwrap(),
            "the failed lookup must not have created the session"
        );
    }

    #[tokio::test]
    async fn session_lifecycle_over_the_handlers() {
        let (_d, srv, _home, _env) = scratch_server();

        let (code, Json(created)) = new_session(State(srv.clone()), None).await;
        assert_eq!(code, StatusCode::CREATED);
        let sid = created["session_id"]
            .as_str()
            .expect("session id")
            .to_string();

        let (_, Json(list)) =
            list_sessions(State(srv.clone()), axum::extract::Query(HashMap::new())).await;
        assert_eq!(list["sessions"].as_array().map(|a| a.len()), Some(1));

        let (code, _) = delete_session(State(srv.clone()), Path(sid.clone())).await;
        assert_eq!(code, StatusCode::OK);
        assert!(!srv.store.lock_ok().session_exists(&sid).unwrap());
    }

    /// Tool rounds must survive into the reloaded history, or the
    /// transcript shows an answer with no trace of the calls that
    /// produced it. USER: "tool invocations were also missing from the
    /// output." The handler used to filter every non-user/assistant row.
    #[tokio::test]
    async fn history_includes_tool_rounds_not_just_answers() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        {
            let st = srv.store.lock_ok();
            st.append_message(
                "s1",
                &veles_core::providers::ChatMessage::user("what is in it?"),
            )
            .unwrap();
            st.append_message(
                "s1",
                &veles_core::providers::ChatMessage::tool_result(
                    "c1",
                    "read_file",
                    "hello world".into(),
                ),
            )
            .unwrap();
            let mut assistant =
                veles_core::providers::ChatMessage::assistant("it says hello world", None);
            // The wire shape: an assistant row whose tool_calls carry the
            // arguments. This is the ONLY persistent copy of them, so
            // the endpoint must hand it back out — a reopened session
            // rendering "⚙ read_file" with no invocation is the bug.
            assistant.tool_calls = Some(serde_json::json!([
                {"id": "c1", "function": {"name": "read_file",
                                            "arguments": "{\"path\":\"/tmp/f\"}"}}
            ]));
            st.append_message("s1", &assistant).unwrap();
        }

        let (code, Json(v)) = history(State(srv.clone()), Path("s1".into())).await;
        assert_eq!(code, StatusCode::OK);
        let msgs = v["messages"].as_array().expect("messages array");

        let tool = msgs
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("a tool row survives history");
        assert_eq!(tool["tool_name"], "read_file");
        assert_eq!(tool["tool_call_id"], "c1", "result joins back to its call");
        assert_eq!(tool["content"], "hello world");
        let assistant = msgs
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("the assistant row survives history");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"], "{\"path\":\"/tmp/f\"}",
            "the invocation rides the assistant row: {v}"
        );
        assert_eq!(msgs.len(), 3, "user, tool and assistant all survive: {v}");
    }

    /// The Tasks pane's single source: todo list + plan mode + the last
    /// presented plan in ONE response (three panels, one poll).
    #[tokio::test]
    async fn todos_carry_the_plan_mode_and_plan_text() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        {
            let st = srv.store.lock_ok();
            st.todos_replace(
                "s1",
                &[veles_core::store::TodoItem {
                    id: "1".into(),
                    content: "do the thing".into(),
                    status: "in_progress".into(),
                }],
            )
            .unwrap();
            veles_core::plan::set_mode(&st, "s1", veles_core::plan::Mode::Plan).unwrap();
            st.meta_set(
                &format!("{}s1", veles_core::plan::PLAN_TEXT_PREFIX),
                "step one, then two",
            )
            .unwrap();
        }

        let Json(v) = session_todos(State(srv.clone()), Path("s1".into())).await;
        assert_eq!(v["plan_mode"], true, "the pane must show the mode it reads");
        assert_eq!(v["plan"], "step one, then two");
        assert_eq!(v["todos"].as_array().unwrap().len(), 1, "{v}");
    }

    /// Cancellation is ONE registry now: the endpoint delegates to
    /// `agent::cancel_turn`, which reads the same process-wide `LIVE_TURNS`
    /// map `run_turn` populates for EVERY surface. The desktop-only
    /// `TurnCancels` map it used to walk is gone — it could not see (or
    /// stop) a turn started by IRC, telegram or cron, and it disagreed
    /// with `cancel_turn` about whether the same session was running.
    #[tokio::test]
    async fn cancel_endpoint_uses_the_one_live_turns_registry() {
        // No turn in flight: 404, not a panic and not a silent 200.
        let (code, Json(v)) = session_cancel(Path("nobody".into())).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(v["error"].is_string(), "{v}");

        // A turn registered exactly as `run_turn` registers it — same
        // `LIVE_TURNS` map — is reachable through the endpoint: the token
        // flips and the answer is 200. This is the integration the old
        // desktop-only map got wrong: it only ever saw desktop turns.
        let sid = "cancel-via-endpoint";
        let tok = CancelToken::new();
        veles_core::agent::register_turn_for_test(sid, tok.clone());
        let (code, Json(v)) = session_cancel(Path(sid.into())).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["cancelling"], 1, "{v}");
        assert!(tok.is_cancelled(), "the endpoint must flip the live token");
        veles_core::agent::clear_turn_for_test(sid);
    }

    /// The detach endpoint's three honest outcomes, same wording
    /// discipline as the chat command so the surfaces cannot disagree:
    /// 404 with nothing running, 409 with a turn that is NOT inside a
    /// foreground command, 200 + flag armed when it is. The middle one
    /// is the safety property: arming a flag between tool rounds would
    /// silently background the NEXT unrelated terminal call.
    #[tokio::test]
    async fn detach_endpoint_answers_honestly_in_all_three_states() {
        // No turn in flight: 404, and no flag was armed behind it.
        let (code, Json(v)) = session_detach(Path("detach-nobody".into())).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(v["error"].is_string(), "{v}");
        assert!(
            !veles_core::agent::detach_requested("detach-nobody"),
            "a refused detach must not arm the flag"
        );

        // A live turn, but not inside a foreground terminal command:
        // 409, flag NOT armed.
        let sid = "detach-live-turn";
        let tok = CancelToken::new();
        veles_core::agent::register_turn_for_test(sid, tok);
        let (code, Json(v)) = session_detach(Path(sid.into())).await;
        assert_eq!(code, StatusCode::CONFLICT, "{v}");
        assert!(
            !veles_core::agent::detach_requested(sid),
            "409 must not arm the flag either"
        );

        // Inside a foreground command: 200, flag armed, and the tool's
        // poll (`detach_requested`) consumes it exactly once.
        veles_core::agent::mark_fg_command(sid, true);
        let (code, Json(v)) = session_detach(Path(sid.into())).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["detaching"], 1, "{v}");
        assert!(
            veles_core::agent::detach_requested(sid),
            "the armed flag reaches the tool's poll"
        );
        assert!(
            !veles_core::agent::detach_requested(sid),
            "and only once — the poll consumes it"
        );
        // Disarming the fg marker clears any residue, so the next turn
        // starts clean.
        veles_core::agent::mark_fg_command(sid, false);
        veles_core::agent::clear_turn_for_test(sid);
    }

    /// Steering routes to the RUNNING turn through the process-global
    /// steer slot (`ask::steer`), never through this server's agent
    /// cache. This is the ROUTING fix for the silent-loss bug: the old
    /// endpoint wrote the `steers` map's Arc, which a second agent
    /// cache for the same session id (the gateway's) never read — 200
    /// "queued" into an Option nobody takes. Now the handle under test
    /// is the one a RUNNING turn published, and it does not even have
    /// to be this server's agent.
    #[tokio::test]
    async fn steer_reaches_the_running_turn_whichever_agent_runs_it() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        let agent = srv.agent_for("s1").expect("agent");

        // A "gateway" agent — a DIFFERENT instance for the same session
        // id, exactly what GatewayCore::sessions holds. Its turn is the
        // one running; the steer must land in ITS pending_steer.
        let gw_agent = crate::fork(&agent, agent.cfg.clone(), "s1", None).expect("fork");
        let gw_handle = gw_agent.pending_steer.clone();
        let _slot = veles_core::ask::claim_steer_slot("s1", gw_handle);

        let (code, Json(v)) = session_steer(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(json!({ "message": "use Python instead" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["state"], json!("live"), "{v}");
        assert_eq!(
            gw_agent.pending_steer.lock_ok().as_deref(),
            Some("use Python instead"),
            "the steer landed in the RUNNING turn's agent, not the desktop cache's"
        );
        assert!(
            agent.pending_steer.lock_ok().is_none(),
            "the idle instance's queue stays empty — one steer, one destination"
        );
    }

    /// No live turn → the steer PARKS (process-global dock) and the next
    /// turn start-injects it as a labeled [STEER] row. The old endpoint
    /// answered 404 "session not active" and the steer was LOST — while
    /// a compaction held the lock with busy:false on /context, that was
    /// the "my /steers never arrive" report.
    /// USER 2026-09-16: *"I DID call /plan on that session."* They did.
    /// Nothing received it.
    ///
    /// `session_plan` peeked `srv.agents` — a CACHE — and answered 404
    /// "session not active" on a miss. But a miss is not an absent
    /// session: it is any session that has not run a turn in THIS
    /// process, and after `reload_config()` (which the config.toml file
    /// watcher fires on every save) it is every session. The desktop
    /// turned that 404 into "plan mode unavailable (gateway offline?)"
    /// on a gateway that was up, so the mode silently stayed off while
    /// the user believed it was on — and plan mode is the one setting
    /// where believing it is on and being wrong is the whole risk.
    ///
    /// Reproduced against the live gateway before the fix:
    /// `POST /api/sessions/<real session>/plan` → 404
    /// `{"error":"session not active"}`.
    #[tokio::test]
    async fn plan_mode_is_set_on_a_session_this_process_has_never_run() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("cold").unwrap();
        // Deliberately NO `agent_for` first: that is the whole point —
        // the session is real and the cache knows nothing about it.
        assert!(
            srv.agents.lock_ok().get("cold").is_none(),
            "the fixture must start with a cold cache or it proves nothing"
        );

        let (code, Json(v)) = session_plan(
            State(srv.clone()),
            Path("cold".to_string()),
            Json(json!({ "mode": "on" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert!(
            v["message"].as_str().unwrap_or_default().contains("ON"),
            "it reports the mode it set: {v}"
        );

        // And it actually took — read it back the way the gate does,
        // not out of the response we just wrote.
        assert_eq!(
            veles_core::plan::mode_for(&srv.store.lock_ok(), "cold"),
            veles_core::plan::Mode::Plan,
            "the store is where plan mode lives; a message saying ON \
             over a store that says off is the bug wearing a smile"
        );

        // A session that genuinely does not exist still 404s — that is
        // the one thing the status code was ever meant to mean.
        let (code, _v) = session_plan(
            State(srv.clone()),
            Path("no-such-session".to_string()),
            Json(json!({ "mode": "on" })),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    /// The same cache peek, the same fix, on the three siblings that
    /// shared it. `/failover` and `/body` answered the same wrong 404;
    /// `/compress` answered "session not active — open it first",
    /// which was advice about a cache the caller cannot see.
    #[tokio::test]
    async fn the_other_per_session_settings_also_work_on_a_cold_session() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("cold2").unwrap();

        let (code, Json(v)) = session_failover(
            State(srv.clone()),
            Path("cold2".to_string()),
            Json(json!({ "mode": "off" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "/failover on a cold session: {v}");

        let (code, Json(v)) = session_extra_body(
            State(srv.clone()),
            Path("cold2".to_string()),
            Json(json!({ "body": "" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "/body on a cold session: {v}");
    }

    #[tokio::test]
    async fn a_steer_with_no_live_turn_parks_and_injects_at_the_next_turn_start() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        let _agent = srv.agent_for("s1").expect("agent");

        let (code, Json(v)) = session_steer(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(json!({ "message": "drop the API idea, use the file" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["state"], json!("parked"), "{v}: the state says parked");

        // The next turn drains the dock at start — take is the same
        // call run_turn makes before its first provider call.
        let docked = veles_core::ask::take_docked_steers("s1");
        assert_eq!(docked, vec!["drop the API idea, use the file".to_string()]);
        // And it does not double-drain.
        assert!(veles_core::ask::take_docked_steers("s1").is_empty());
    }

    /// The incident itself (PENDING 2026-09-14, store-reproduced): a
    /// desktop turn runs to completion on a session whose STANDING
    /// asker (the gateway's ChatAsker, attached at agent build) is
    /// live. After the desktop turn's cleanup, `clarify` on that
    /// session must still find a human — the old single-slot registry
    /// was emptied by the turn's `clear_asker` and `clarify` answered
    /// `no_human()` while the owner watched the screen.
    #[tokio::test]
    async fn a_finished_desktop_turn_does_not_forget_the_standing_human() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        // The gateway's standing attach, as agent_for_sender does.
        let standing = std::sync::Arc::new(veles_core::ask::ChannelAsker {
            tx: {
                let (tx, _rx) = std::sync::mpsc::channel();
                tx
            },
        });
        veles_core::ask::set_session_asker("s1", standing.clone());
        // The desktop's asker (attached by agent_for) + a full turn
        // lifecycle's set/clear pair, which no longer exists — simulate
        // the OLD bug's shape by detaching ONLY the desktop token, as
        // the fixed cleanup does.
        let agent = srv.agent_for("s1").expect("agent");
        let desktop_id = agent.set_asker(std::sync::Arc::new(veles_core::ask::PolicyAsker {
            approve: false,
        }));
        assert!(agent.asker().is_some());
        agent.detach_asker(desktop_id);

        // clarify's exact check: legs via the registry. The standing
        // asker must survive the desktop turn's cleanup.
        let legs = veles_core::ask::session_askers("s1");
        assert!(
            legs.iter().any(|a| a.interactive()),
            "a desktop turn ending must not erase the standing route to the human"
        );
        // The API the tool actually calls:
        let req = veles_core::ask::AskRequest::new(
            "s1",
            veles_core::ask::AskKind::Clarify,
            "which file?",
            vec![],
        )
        .with_timeout(5);
        // Spawn the ask, answer it from the standing channel, and assert
        // the turn receives the human's words — not `no_human()`.
        let (tx, rx) = std::sync::mpsc::channel();
        let standing2 = std::sync::Arc::new(veles_core::ask::ChannelAsker { tx });
        veles_core::ask::set_session_asker("s1", standing2);
        let handle =
            veles_core::rt::spawn(async move { veles_core::ask::ask_session("s1", req).await });
        // The spawn must RUN before the blocking recv: on the default
        // single-threaded test runtime the blocked main task would
        // otherwise wait out the recv window with the spawn still
        // queued (Timeout, not Disconnected — the pin is what fixed
        // the Disconnected half).
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if let Ok(a) = rx.try_recv() {
                let asked = a;
                assert!(veles_core::ask::resolve(
                    &asked.id,
                    veles_core::ask::AskAnswer::text("main.tcl")
                ));
                break;
            }
        }
        let answer = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("ask resolves")
            .unwrap()
            .expect("a human was reachable");
        assert_eq!(answer.free_text.as_deref(), Some("main.tcl"));
        // Teardown (test-owns-the-fixture spelling).
        veles_core::ask::clear_session_asker("s1");
    }

    /// Relay: one ask, two surfaces. Both render it (two legs under one
    /// id), the FIRST answer wins and EVERY leg resolves — the losing
    /// surface's dialog closes on its own. This is the multi-subscriber
    /// asker contract.
    #[tokio::test]
    async fn one_ask_many_surfaces_first_answer_closes_all_legs() {
        let r = veles_core::ask::AskRequest::new(
            "s-relay",
            veles_core::ask::AskKind::Approval,
            "run rm?",
            veles_core::ask::approval_choices(true),
        );
        // The desktop leg (bus/REST-shaped) and a chat leg join the
        // SAME id.
        let leg_a = veles_core::ask::register(&r, None, None).expect("first leg");
        let leg_b = veles_core::ask::register(&r, Some("telegram:1"), Some("telegram:7"))
            .expect("second leg");
        let a = veles_core::rt::spawn(async move { leg_a.wait().await });
        let b = veles_core::rt::spawn(async move { leg_b.wait().await });

        // The chat's human answers first.
        assert!(veles_core::ask::resolve_for_chat("telegram:1", "1", Some("telegram:7")).is_some());
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(a.first(), Some(veles_core::ask::CHOICE_ONCE));
        assert_eq!(
            b.first(),
            Some(veles_core::ask::CHOICE_ONCE),
            "the losing surface's leg resolves too — its dialog closes"
        );
        // Single-shot: the id is spent.
        assert!(!veles_core::ask::resolve(
            &r.id,
            veles_core::ask::AskAnswer::choice(veles_core::ask::CHOICE_DENY)
        ));
        assert!(veles_core::ask::register(&r, None, None).is_none());
    }

    /// A relayed CHAT leg tightens the ask's bindings: a desktop-headed
    /// ask that reached a channel is NOT answerable by a random channel
    /// member (S15), and the binding-join must not loosen it back.
    #[tokio::test]
    async fn a_relayed_chat_leg_tightens_answer_bindings() {
        // The answer-binding check reads the live ACL, so this test
        // must own it: a neighbour's scratch home (no config.toml =>
        // dormant ACL => every sender resolves Owner) is exactly the
        // flake this guard + snapshot pin removes. Same shape as
        // `the_ask_verbs_are_bound_to_whoever_opened_the_ask`.
        let _g = veles_core::testenv::guard();
        veles_core::ask::set_acl_snapshot_for_tests(Some(
            serde_yaml::from_str("owner: [\"irc:boss\"]").unwrap(),
        ));
        let r = veles_core::ask::AskRequest::new(
            "s-tighten",
            veles_core::ask::AskKind::Approval,
            "run rm?",
            veles_core::ask::approval_choices(true),
        );
        let _desktop_leg = veles_core::ask::register(&r, None, None).expect("open");
        let chat = veles_core::testenv::chat_key();
        let bound = format!("{chat}-42");
        let stranger = format!("{chat}-999");
        let _chat_leg = veles_core::ask::register(&r, Some(&chat), Some(&bound)).expect("join");
        // A stranger in the channel cannot answer by number.
        assert!(veles_core::ask::resolve_for_chat(&chat, "1", Some(&stranger)).is_none());
        // The bound sender can.
        assert!(veles_core::ask::resolve_for_chat(&chat, "1", Some(&bound)).is_some());
        veles_core::ask::set_acl_snapshot_for_tests(None);
    }

    /// Overlapping turn lifecycles keep independent cancel
    /// registrations: the first to end must not unregister the second's
    /// token (the LIVE_TURNS singleton was the same one-per-session bug
    /// as the asker slot).
    #[tokio::test]
    async fn overlapping_turns_keep_their_own_cancel_registrations() {
        let t1 = veles_core::providers::CancelToken::new();
        let t2 = veles_core::providers::CancelToken::new();
        veles_core::agent::register_turn_for_test("s-overlap", t1.clone());
        veles_core::agent::register_turn_for_test("s-overlap", t2.clone());
        // Turn 1 ends (its guard removes only its own token).
        let g = veles_core::agent::turn_guard_for_test("s-overlap", t1);
        drop(g);
        assert!(
            veles_core::agent::turn_is_live("s-overlap"),
            "turn 2 is still live"
        );
        assert!(veles_core::agent::cancel_turn("s-overlap"));
        assert!(t2.is_cancelled());
        // C3: cancel_turn flips IN PLACE — the turn is still unwinding
        // (a cancelled turn persists its partial content and its guard
        // drops at the end), so `turn_is_live` stays true until the
        // guard removes the registration. This used to assert the
        // opposite, pinning the remove-first shape that made every
        // map-reading poller (the terminal tool's foreground wait, the
        // remote-task cancel watch) read "not cancelled" at the exact
        // moment Stop was pressed.
        assert!(
            veles_core::agent::turn_is_live("s-overlap"),
            "cancelling does not unregister a turn that is still unwinding"
        );
        let g2 = veles_core::agent::turn_guard_for_test("s-overlap", t2);
        drop(g2);
        assert!(
            !veles_core::agent::turn_is_live("s-overlap"),
            "the guard's drop is what unregisters"
        );
    }

    /// The four queue endpoints touch the SAME store rows the TUI's
    /// composer and the `/queue` command do — one table, no private
    /// copies (R4.6). Pending rows edit and cancel; the list shows
    /// state, so a client can tell parked from running.
    #[tokio::test]
    async fn queue_endpoints_share_the_store_with_every_surface() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();

        let (code, Json(v)) = queue_push(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(QueuePushRequest {
                body: "alpha".into(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let seq = v["seq"].as_i64().expect("seq");

        let (code, Json(v)) = queue_view(State(srv.clone()), Path("s1".to_string())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["items"][0]["body"], "alpha");
        assert_eq!(v["items"][0]["state"], "pending");

        // Edit before it runs.
        let (code, _) = queue_patch(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(QueueMutateRequest {
                seq,
                body: Some("alpha beta".into()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            srv.store.lock_ok().queue_items("s1").unwrap()[0].body,
            "alpha beta"
        );

        // Cancel.
        let (code, _) = queue_delete(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(QueueMutateRequest { seq, body: None }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(srv.store.lock_ok().queue_items("s1").unwrap().is_empty());

        // Cancelling an already-gone row is a 404, honest about it.
        let (code, _) = queue_delete(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(QueueMutateRequest { seq, body: None }),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    /// POST /api/chat while a turn is live does not race it and does
    /// not drop the text: the message becomes a queue row, drained at
    /// the turn boundary. The live check rides the store's
    /// `session_runs` — the same fact /api/sessions reports as
    /// `running` — so a TUI turn queues a desktop message too.
    /// A turn for a session that migrated to another node is answered
    /// with where it lives — not started, not refused by the fence with a
    /// message about rows being "archived".
    #[tokio::test]
    async fn chat_for_a_session_that_lives_elsewhere_names_its_node() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        srv.store
            .lock_ok()
            .conn()
            .execute(
                "UPDATE sessions SET node = 'migrated_to:node-x@1' WHERE id = 's1'",
                [],
            )
            .unwrap();
        let resp = chat(
            State(srv.clone()),
            Json(ChatRequest {
                session_id: "s1".into(),
                join_id: None,
                text: "hello".into(),
                attachments: vec![],
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["owner"], "node-x");
        assert!(
            v["error"].as_str().unwrap_or("").contains("lives on node"),
            "{v}"
        );
        assert!(srv.store.lock_ok().queue_items("s1").unwrap().is_empty());
    }

    #[tokio::test]
    async fn chat_during_a_live_turn_queues_instead_of_racing() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        srv.store
            .lock_ok()
            .run_start("s1", std::process::id(), "box", "tui")
            .unwrap();

        let resp = chat(
            State(srv.clone()),
            Json(ChatRequest {
                session_id: "s1".into(),
                join_id: None,
                text: "while busy".into(),
                attachments: vec![],
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let items = srv.store.lock_ok().queue_items("s1").unwrap();
        assert_eq!(
            items.len(),
            1,
            "the message must park, not start: {items:?}"
        );
        assert_eq!(items[0].body, "while busy");
        assert_eq!(items[0].state, "pending");
    }

    /// `/clear` still archives everything; `/compress` SUMMARIZES via the
    /// agent, and it BUILDS one for a session this process has not
    /// happened to touch.
    ///
    /// It used to refuse with 400 "session not active — open it first"
    /// on a cache miss, which was advice about a cache the caller
    /// cannot see: a miss is any session that has not run a turn in
    /// this process, and `reload_config()` empties the map for all of
    /// them on every config.toml save. Same defect as `/plan`'s 404
    /// (see `plan_mode_is_set_on_a_session_this_process_has_never_run`),
    /// and it is refused now only when the SESSION does not exist.
    /// /clear on a DAMAGED session — the one a user reaches for when the
    /// provider keeps answering 400. The count used to come from the
    /// repaired `load_history`, which hides orphaned tool results, so
    /// `archive_head(count)` archived the orphan first and left the
    /// NEWEST row live: "cleared", and the last reply still in context.
    #[tokio::test]
    async fn clear_on_a_session_with_an_orphaned_result_clears_everything() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        {
            let st = srv.store.lock_ok();
            st.append_message("s1", &veles_core::providers::ChatMessage::user("go"))
                .unwrap();
            st.append_message(
                "s1",
                &veles_core::providers::ChatMessage::tool_result(
                    "call_gone",
                    "read_file",
                    "orphan".into(),
                ),
            )
            .unwrap();
            st.append_message(
                "s1",
                &veles_core::providers::ChatMessage::assistant("the last reply", None),
            )
            .unwrap();
        }
        let (code, Json(v)) = session_clear(State(srv.clone()), Path("s1".into())).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["archived"], 3, "every live row, the hidden one included");
        let live = srv
            .store
            .lock_ok()
            .load_history_since("s1", 0)
            .expect("raw live set");
        assert!(live.is_empty(), "nothing survives a clear: {live:?}");
    }

    /// A note pinned after a message renders right after THAT message,
    /// even when an earlier row is one the wire repair would hide. The
    /// repaired vector zipped against unrepaired ids shifted every row
    /// after the orphan by one, and the note landed a message late.
    /// And a cursor between a call and its result still serves the
    /// result.
    #[tokio::test]
    async fn history_rows_keep_notes_anchored_and_serve_a_split_round() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "anchored";
        srv.store.lock_ok().create_session(sid).unwrap();
        let call_id = {
            let st = srv.store.lock_ok();
            st.append_message(sid, &veles_core::providers::ChatMessage::user("u1"))
                .unwrap();
            st.append_message(
                sid,
                &veles_core::providers::ChatMessage::tool_result(
                    "call_gone",
                    "read_file",
                    "orphan".into(),
                ),
            )
            .unwrap();
            st.append_message(
                sid,
                &veles_core::providers::ChatMessage::assistant("a1", None),
            )
            .unwrap();
            st.record_note(sid, "cron", "pinned after a1", None, None)
                .unwrap();
            st.append_message(sid, &veles_core::providers::ChatMessage::user("u2"))
                .unwrap();
            let mut call = veles_core::providers::ChatMessage::assistant("", None);
            call.tool_calls = Some(serde_json::json!([{
                "id": "c9",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{}"}
            }]));
            let call_id = st.append_message(sid, &call).unwrap();
            st.append_message(
                sid,
                &veles_core::providers::ChatMessage::tool_result("c9", "read_file", "late".into()),
            )
            .unwrap();
            call_id
        };
        let rows = history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, 0, 0).expect("rows");
        let pos = |pred: &dyn Fn(&Value) -> bool| rows.iter().position(pred);
        let a1 = pos(&|r| r["content"] == "a1").expect("a1 rendered");
        let note = pos(&|r| r["role"] == "note").expect("note rendered");
        assert_eq!(
            note,
            a1 + 1,
            "the note follows a1, not a later row: {rows:#?}"
        );

        let gap =
            history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, call_id, 0).expect("gap");
        assert!(
            gap.iter().any(|r| r["content"] == "late"),
            "the result past the cursor is served even though its call is behind it: {gap:#?}"
        );
    }

    #[tokio::test]
    async fn clear_archives_and_compress_works_on_a_session_with_no_agent_yet() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        {
            let st = srv.store.lock_ok();
            for i in 0..6 {
                st.append_message(
                    "s1",
                    &veles_core::providers::ChatMessage::user(&format!("msg {i}")),
                )
                .unwrap();
            }
        }

        // No cached agent, but a real session: accepted. (The summarizer
        // runs in a spawned task against a dead-port provider here — the
        // point is the DOOR, not the summary.)
        assert!(
            srv.agents.lock_ok().get("s1").is_none(),
            "cache starts cold"
        );
        let (code, Json(v)) = session_compress(State(srv.clone()), Path("s1".into())).await;
        assert_eq!(code, StatusCode::OK, "{v}");

        // A session that does not exist is the one real refusal.
        let (code, _v) = session_compress(State(srv.clone()), Path("no-such-session".into())).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        // /clear is unchanged: archive every live row, keep searchable.
        let (code, Json(v)) = session_clear(State(srv.clone()), Path("s1".into())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["archived"], 6, "whatever remained");
        // Messages only: the compress above may have written its note by
        // now ("compact did not run"), since its task gets to run while
        // the history read is off on the blocking pool.
        let (_, Json(h)) = history(State(srv.clone()), Path("s1".into())).await;
        let rows = h["messages"].as_array().unwrap();
        assert!(
            rows.iter().all(|r| r["role"] == "note"),
            "cleared context loads empty: {h}"
        );
    }

    /// Drain the session bus until the expected `history-changed` event
    /// for `sid` arrives (skipping other sessions' traffic — the bus is
    /// process-wide and tests share it), then check its `reason`. A
    /// missing event fails fast rather than hanging the suite.
    async fn expect_history_changed(
        rx: &mut tokio::sync::broadcast::Receiver<veles_core::session_tap::SessionEvent>,
        sid: &str,
        reason: &str,
    ) -> serde_json::Value {
        let wait = async {
            loop {
                let ev = rx.recv().await.expect("history-changed never arrived");
                if ev.session_id == sid {
                    assert_eq!(ev.kind, "history-changed", "{ev:?}");
                    assert_eq!(ev.data["reason"], reason, "{ev:?}");
                    return ev.data;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for history-changed({sid}, {reason})"))
    }

    /// `history-changed` rides the session bus for EVERY transcript
    /// mutation outside a turn — `/clear`, `/undo`, `/branch`. Clients
    /// reload on this event instead of divining mid-operation liveness
    /// from `/context`'s `busy` key, which structurally cannot carry
    /// any of it (see the 2026-09-07 incident in
    /// docs/issues/reference/PENDING.md: the key is on
    /// every answer, so key-absence gates in Tcl were dead code, and a
    /// compaction holds the agent lock yet is not a turn, so the value
    /// cannot mean "finished" either).
    #[tokio::test]
    async fn transcript_mutations_publish_history_changed() {
        let (_d, srv, _home, _env) = scratch_server();
        let mut rx = veles_core::session_tap::subscribe();
        let sid = "hist-changed-s1";
        srv.store.lock_ok().create_session(sid).unwrap();
        for i in 0..3 {
            srv.store
                .lock_ok()
                .append_message(
                    sid,
                    &veles_core::providers::ChatMessage::user(&format!("msg {i}")),
                )
                .unwrap();
        }

        let (code, Json(v)) = session_clear(State(srv.clone()), Path(sid.to_string())).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let d = expect_history_changed(&mut rx, sid, "clear").await;
        assert_eq!(
            d["last_msg_id"], 3,
            "the store head after the clear, as the resume cursor: {d}"
        );

        // /undo on a session cleared empty is not a mutation worth the
        // event — put two fresh rows back first.
        for i in 4..=5 {
            srv.store
                .lock_ok()
                .append_message(
                    sid,
                    &veles_core::providers::ChatMessage::user(&format!("msg {i}")),
                )
                .unwrap();
        }
        let (code, Json(v)) = session_undo(
            State(srv.clone()),
            Path(sid.to_string()),
            Json(json!({ "n": 1 })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        expect_history_changed(&mut rx, sid, "undo").await;

        let (code, Json(v)) = session_branch(State(srv.clone()), Path(sid.to_string())).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let new_id = v["new_session_id"].as_str().unwrap().to_string();
        expect_history_changed(&mut rx, &new_id, "branch").await;
    }

    /// `/profile` used to hardcode "default" in Tcl; the endpoint asks
    /// the engine. In a scratch home no profiles dir exists, so the
    /// active profile is "default" and the only entry.
    #[tokio::test]
    async fn profile_view_reports_the_active_profile() {
        let (_d, srv, _home, _env) = scratch_server();
        let Json(v) = profile_view(State(srv.clone())).await;
        assert_eq!(v["active"], "default");
        let profiles = v["profiles"].as_array().expect("profiles array");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0]["name"], "default");
        assert_eq!(profiles[0]["active"], true);
    }

    /// The drain loop, end to end on the real endpoint.
    ///
    /// Subscribing happens when the handler is CALLED; nothing is polled
    /// until the body is read. A burst published in between is therefore
    /// exactly the backlog a client that has fallen behind faces — which
    /// is the situation stream_pacing measured on the wire (766 SSE
    /// frames inside one 239ms window), and the only situation in which
    /// merging is supposed to happen at all.
    #[tokio::test]
    async fn a_burst_of_deltas_leaves_as_merged_frames_carrying_every_character() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("burst").unwrap();

        let sse = session_events(
            State(srv.clone()),
            Path("burst".to_string()),
            Query(SessionEventsQuery {
                since: 0,
                since_note: 0,
            }),
        )
        .await;

        let chunks: Vec<String> = (0..400).map(|i| format!("t{i} ")).collect();
        let whole: String = chunks.concat();
        for c in &chunks {
            veles_core::session_tap::publish_stream(
                "burst",
                &veles_core::providers::StreamEvent::Delta(c.clone()),
            );
        }

        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut got = String::new();
        let mut delta_frames = 0usize;
        while got.len() < whole.len() {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
                .await
                .expect("frames within 5s")
                .expect("body still open")
                .expect("bytes");
            raw.push_str(&String::from_utf8_lossy(&chunk));
            // SSE records are separated by a blank line; keep any partial
            // record for the next read.
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                let mut kind = "";
                let mut data = "";
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = v.trim();
                    }
                }
                if kind == "delta" {
                    delta_frames += 1;
                    let v: serde_json::Value = serde_json::from_str(data).expect("delta json");
                    got.push_str(v["text"].as_str().expect("text"));
                }
            }
        }
        // Not one character lost or reordered: merging changes FRAMING.
        assert_eq!(got, whole);
        // And it actually merged — otherwise this is just the old path
        // passing a test that names the new one.
        assert!(
            delta_frames < chunks.len() / 4,
            "400 backlogged deltas should drain as far fewer frames, got {delta_frames}"
        );
    }

    /// The phone's notifier (USER 2026-09-30: "Asks and finished turns")
    /// hears an ask of any turn but one IRC or Telegram started (they
    /// notify on their own), its resolution, and the end of a turn the
    /// desktop started, not of one a cron job or a chat started.
    #[test]
    fn the_notifier_hears_asks_and_the_turns_the_desktop_started() {
        use veles_core::session_tap::SessionEvent;
        let ev = |kind: &str, data: Value| SessionEvent {
            seq: 1,
            session_id: "s".into(),
            kind: kind.into(),
            data,
        };
        let ask = ev(
            "ask",
            json!({"id": "a1", "kind": "approval", "prompt": "rm -rf?"}),
        );
        let (name, data) = notify_frame(&ask, Some("desktop")).expect("a desktop turn's ask");
        assert_eq!(name, "ask");
        assert_eq!(
            (data["id"].as_str(), data["prompt"].as_str()),
            (Some("a1"), Some("rm -rf?"))
        );
        assert_eq!(data["session_id"], "s");
        assert!(
            notify_frame(&ask, Some("cron")).is_some(),
            "a cron turn's ask reaches nobody else"
        );
        assert!(notify_frame(&ask, None).is_some());
        assert!(
            notify_frame(&ask, Some("telegram")).is_none(),
            "telegram notifies its own"
        );
        assert!(notify_frame(&ask, Some("irc")).is_none(), "so does IRC");
        let stamped = ev("ask", json!({"id": "a2", "surface": "telegram"}));
        assert!(
            notify_frame(&stamped, None).is_none(),
            "the ask's own surface decides"
        );
        let (name, data) = notify_frame(&ev("ask-resolved", json!({"id": "a1"})), None).unwrap();
        assert_eq!((name, data["id"].as_str()), ("ask-resolved", Some("a1")));
        let done = |surface: &str| {
            notify_frame(&ev("turn-ended", json!({ "surface": surface })), None).map(|f| f.0)
        };
        assert_eq!(done("desktop"), Some("turn-done"));
        assert_eq!(done("cron"), None);
        assert_eq!(done("telegram"), None);
        assert!(notify_frame(&ev("delta", json!({"text": "x"})), Some("desktop")).is_none());
        // What a notification says of the answer: plain text.
        assert_eq!(
            notify_plain("L1 inline [the docs](https://example.com/docs) **here**."),
            "L1 inline the docs here."
        );
        assert_eq!(notify_plain("# Heading `code`"), "Heading code");
        assert_eq!(notify_plain("- a bullet"), "a bullet");
    }

    /// The stream itself: an ask already open when it connects, a new
    /// one with the session's title, its resolution, and a finished
    /// desktop turn with the first line of the answer. A telegram turn's
    /// ask never reaches it.
    #[tokio::test]
    async fn the_notify_stream_carries_asks_and_finished_turns_with_their_titles() {
        use futures::StreamExt;
        let _serial = NOTIFY_SERIAL.lock().await;
        let (_d, srv, _home, _env) = scratch_server();
        {
            let store = srv.store.lock_ok();
            store.create_session("notify-d").unwrap();
            store.set_title("notify-d", "the build").unwrap();
            store.create_session("notify-t").unwrap();
        }
        veles_core::session_tap::turn_started("notify-d", "desktop", 0, "clean it");
        veles_core::session_tap::turn_started("notify-t", "telegram", 0, "hi");
        // The missed turns are process-global: a parallel test's finished
        // desktop turn must not inject an extra turn-done into this exact
        // sequence. Cleared here, immediately before connecting.
        veles_core::session_tap::clear_missed_turns();
        let sse = notify_events(State(srv.clone())).await;
        let pubask = |sid: &str, prompt: &str| {
            let req = veles_core::ask::AskRequest::new(
                sid,
                veles_core::ask::AskKind::Approval,
                prompt,
                veles_core::ask::approval_choices(false),
            );
            veles_core::session_tap::publish_stream(
                sid,
                &veles_core::providers::StreamEvent::Ask(Box::new(req.clone())),
            );
            req.id
        };
        pubask("notify-t", "not for the phone");
        let d_ask = pubask("notify-d", "delete the build dir?");
        veles_core::session_tap::publish("notify-d", "ask-resolved", json!({ "id": d_ask }));
        srv.store
            .lock_ok()
            .append_message(
                "notify-d",
                &veles_core::providers::ChatMessage::assistant(
                    "Done: the build is clean.\nMore.",
                    None,
                ),
            )
            .unwrap();
        veles_core::session_tap::turn_ended("notify-t");
        veles_core::session_tap::turn_ended("notify-d");

        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut seen: Vec<(String, Value)> = Vec::new();
        while !seen.iter().any(|(k, _)| k == "turn-done") {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
                .await
                .expect("frames within 5s")
                .expect("body still open")
                .expect("bytes");
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                let mut kind = String::new();
                let mut data = Value::Null;
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = serde_json::from_str(v.trim()).unwrap_or(Value::Null);
                    }
                }
                // Other tests share the process-wide bus.
                if data["session_id"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("notify-"))
                {
                    seen.push((kind, data));
                }
            }
        }
        let kinds: Vec<&str> = seen.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, vec!["ask", "ask-resolved", "turn-done"], "{seen:?}");
        assert_eq!(seen[0].1["id"], d_ask.as_str());
        assert_eq!(seen[0].1["prompt"], "delete the build dir?");
        assert_eq!(seen[0].1["title"], "the build");
        assert_eq!(seen[1].1["id"], d_ask.as_str());
        assert_eq!(seen[2].1["session_id"], "notify-d");
        assert_eq!(seen[2].1["text"], "Done: the build is clean.");
    }

    /// The notify tests share process-global state (the bus, the missed
    /// turns, the listener count): one at a time, or one test's open
    /// stream makes another's "ended while nobody listened" a lie.
    static NOTIFY_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The (event, data) frames a notify stream sends within `ms` — the
    /// replay is sent at once, so a short quiet spell means it is over.
    async fn notify_frames(
        body: &mut (impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
        ms: u64,
    ) -> Vec<(String, serde_json::Value)> {
        use futures::StreamExt;
        let mut raw = String::new();
        let mut out = Vec::new();
        while let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(std::time::Duration::from_millis(ms), body.next()).await
        {
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                let mut kind = String::new();
                let mut data = serde_json::Value::Null;
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = serde_json::from_str(v.trim()).unwrap_or(serde_json::Value::Null);
                    }
                }
                if !kind.is_empty() {
                    out.push((kind, data));
                }
            }
        }
        out
    }

    /// A desktop turn that ended while NO notifier listened is announced
    /// when one connects — once. A turn a connected notifier saw live is
    /// never replayed: it was posted then, or skipped because the desktop
    /// was in front and showed it, and a later reconnect must not post
    /// it (again).
    #[tokio::test]
    async fn notify_events_replays_only_a_turn_nobody_heard_end() {
        let _serial = NOTIFY_SERIAL.lock().await;
        let (_d, srv, _home, _env) = scratch_server();
        let (away, heard) = ("notify-away", "notify-heard");
        for (sid, text) in [
            (away, "finished while you were away"),
            (heard, "heard live"),
        ] {
            let store = srv.store.lock_ok();
            store.create_session(sid).unwrap();
            store
                .append_message(
                    sid,
                    &veles_core::providers::ChatMessage::assistant(text, None),
                )
                .unwrap();
        }
        let done_for = |frames: &[(String, serde_json::Value)], sid: &str| {
            frames
                .iter()
                .filter(|(k, d)| k == "turn-done" && d["session_id"].as_str() == Some(sid))
                .map(|(_, d)| d.clone())
                .collect::<Vec<_>>()
        };
        // Ends NOW, before any notify stream exists.
        veles_core::session_tap::turn_started(away, "desktop", 0, "do it");
        veles_core::session_tap::turn_ended(away);

        let first = notify_events(State(srv.clone())).await;
        let mut body = first.into_response().into_body().into_data_stream();
        let replayed = notify_frames(&mut body, 300).await;
        let got = done_for(&replayed, away);
        assert_eq!(got.len(), 1, "the missed turn is replayed: {replayed:?}");
        assert_eq!(
            got[0]["text"].as_str(),
            Some("finished while you were away")
        );
        // This one ends while the notifier listens: it goes out live.
        veles_core::session_tap::turn_started(heard, "desktop", 0, "and this");
        veles_core::session_tap::turn_ended(heard);
        let live = notify_frames(&mut body, 300).await;
        assert_eq!(done_for(&live, heard).len(), 1, "heard live: {live:?}");
        drop(body);

        // The reconnect: neither is posted again.
        let second = notify_events(State(srv.clone())).await;
        let mut body = second.into_response().into_body().into_data_stream();
        let again = notify_frames(&mut body, 300).await;
        assert!(
            done_for(&again, away).is_empty() && done_for(&again, heard).is_empty(),
            "a reconnect replays nothing already handed over or heard: {again:?}"
        );
    }

    /// The ordering guarantee the merge exists inside of: a tool round
    /// published in the middle of a burst must still arrive BETWEEN the
    /// text that preceded it and the text that followed.
    #[tokio::test]
    async fn a_tool_round_inside_a_burst_still_lands_between_the_text_around_it() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("order").unwrap();

        let sse = session_events(
            State(srv.clone()),
            Path("order".to_string()),
            Query(SessionEventsQuery {
                since: 0,
                since_note: 0,
            }),
        )
        .await;

        for c in ["a", "b", "c"] {
            veles_core::session_tap::publish_stream(
                "order",
                &veles_core::providers::StreamEvent::Delta(c.into()),
            );
        }
        veles_core::session_tap::publish_stream(
            "order",
            &veles_core::providers::StreamEvent::ToolStart("read_file".into(), "{}".into()),
        );
        for c in ["x", "y", "z"] {
            veles_core::session_tap::publish_stream(
                "order",
                &veles_core::providers::StreamEvent::Delta(c.into()),
            );
        }

        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut seen: Vec<String> = Vec::new();
        while !seen.iter().any(|s| s.starts_with("delta:xyz")) {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
                .await
                .expect("frames within 5s")
                .expect("body still open")
                .expect("bytes");
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                let mut kind = "";
                let mut data = "";
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = v.trim();
                    }
                }
                match kind {
                    "delta" => {
                        let v: serde_json::Value = serde_json::from_str(data).expect("json");
                        seen.push(format!("delta:{}", v["text"].as_str().expect("text")));
                    }
                    "tool" => seen.push("tool".into()),
                    _ => {}
                }
            }
        }
        // The three before fold together, the three after fold together,
        // and the tool round keeps its place between them.
        assert_eq!(seen, vec!["delta:abc", "tool", "delta:xyz"]);
    }

    /// POST /api/chat is FIRE-AND-FORGET: it starts the turn and returns
    /// 202 at once, with no request-scoped stream to hold or lose. The
    /// turn's events ride the session bus and the client watches them on
    /// GET /api/sessions/:id/events like any other viewer — which is the
    /// structural half of "a second window can watch a turn" and "a
    /// reconnect can rejoin one": there is no single-consumer channel left
    /// to be the only copy.
    #[tokio::test]
    async fn chat_is_fire_and_forget_and_refuses_an_unknown_session() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("ff-ok").unwrap();

        // A real session: 202 Accepted, not an SSE body. The spawned turn
        // has no provider here and errors out on the bus; the contract
        // under test is the IMMEDIATE ack (the handler does not await the
        // turn), so a hanging or failing provider cannot wedge it.
        let resp = chat(
            State(srv.clone()),
            Json(ChatRequest {
                session_id: "ff-ok".into(),
                join_id: None,
                text: "hello".into(),
                attachments: vec![],
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // An unknown session: 400, because agent_for refuses to invent one
        // (the desktop never creates sessions implicitly).
        let resp = chat(
            State(srv.clone()),
            Json(ChatRequest {
                session_id: "ff-nope".into(),
                join_id: None,
                text: "hello".into(),
                attachments: vec![],
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `/retry` used to REFUSE to start a turn — its own comment said
    /// why: "Spawning a turn directly here would need to wire up the SSE
    /// event stream, which is session-scoped." The bus retired that
    /// reason; retry is now the same fire-and-forget spawn `chat()` is,
    /// off the prompt `run_turn` remembered at its own choke point.
    #[tokio::test]
    async fn retry_starts_the_turn_instead_of_handing_the_prompt_back() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("retry-ok").unwrap();

        // No provider in this environment: the turn errors on the bus,
        // but run_turn has remembered the prompt by then. Drive one turn
        // through chat() and wait for its verdict so the retry below is
        // not racing the spawn.
        let mut bus = veles_core::session_tap::subscribe();
        let resp = chat(
            State(srv.clone()),
            Json(ChatRequest {
                session_id: "retry-ok".into(),
                join_id: None,
                text: "hello".into(),
                attachments: vec![],
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let mut saw_error = false;
        for _ in 0..512 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), bus.recv())
                .await
                .expect("bus event within 5s")
                .expect("bus open");
            if ev.session_id == "retry-ok" && ev.kind == "turn-error" {
                saw_error = true;
                break;
            }
        }
        assert!(
            saw_error,
            "the turn ran and errored: the prompt is remembered"
        );

        // Retry re-runs it: 202 and a turn on the bus, same contract as
        // chat — not a 200 handing the prompt back for the client to
        // babysit.
        let mut bus2 = veles_core::session_tap::subscribe();
        let (code, _body) = session_retry(State(srv.clone()), Path("retry-ok".into())).await;
        assert_eq!(code, StatusCode::ACCEPTED);
        let mut restarted = false;
        for _ in 0..512 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), bus2.recv())
                .await
                .expect("bus event within 5s")
                .expect("bus open");
            if ev.session_id == "retry-ok" && ev.kind == "turn-started" {
                restarted = true;
                break;
            }
        }
        assert!(restarted, "the retried turn actually started");

        // A session with no turn yet has nothing to retry: 400.
        srv.store.lock_ok().create_session("retry-empty").unwrap();
        let (code, _body) = session_retry(State(srv.clone()), Path("retry-empty".into())).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }

    /// An approval outlives the window that started the turn: it rides the
    /// session bus AND parks in the ask registry, so a client that has
    /// gone away does NOT auto-deny it.
    ///
    /// This is the "closing the desktop mid-turn silently denied the next
    /// approval" bug. The old asker sent the question down the one
    /// per-request channel and, the moment that send failed, CANCELLED the
    /// ask and answered it with a timeout — a closed window deciding the
    /// turn's fate. There is no per-request channel at all now: the bus is
    /// the only path, and the registry holds the ask until someone answers
    /// over REST.
    #[tokio::test]
    async fn an_approval_rides_the_bus_and_survives_a_closed_window() {
        use veles_core::ask::Asker as _;
        let sid = "ask-survives-closed-window";
        let asker = DesktopAsker {
            session_id: sid.into(),
        };

        let mut bus = veles_core::session_tap::subscribe();
        let req = veles_core::ask::AskRequest::new(
            sid,
            veles_core::ask::AskKind::Approval,
            "run rm -rf /tmp/x?",
            veles_core::ask::approval_choices(false),
        );
        let req_for_task = req.clone();
        let handle = veles_core::rt::spawn(async move { asker.ask(req_for_task).await });

        // The ask reaches the bus for whoever is watching now.
        let mut saw_ask = false;
        for _ in 0..256 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv())
                .await
                .expect("bus event within 2s")
                .expect("bus open");
            if ev.session_id == sid && ev.kind == "ask" {
                saw_ask = true;
                break;
            }
        }
        assert!(saw_ask, "the approval must ride the session bus");

        // …and it is parked in the registry for whoever reattaches later,
        // NOT auto-denied despite the dead stream. This is the assertion
        // that would have failed before the fix.
        assert!(
            veles_core::ask::open_all().iter().any(|r| r.id == req.id),
            "a closed window is not an answer: the ask must stay open"
        );

        // Answering over REST — exactly what a reattached client does after
        // picking the ask up from GET /api/asks — resolves the parked turn.
        assert!(veles_core::ask::resolve(
            &req.id,
            veles_core::ask::AskAnswer::choice(veles_core::ask::CHOICE_ONCE)
        ));
        let ans = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("ask returns once answered")
            .expect("ask task did not panic");
        assert_eq!(
            ans.first(),
            Some(veles_core::ask::CHOICE_ONCE),
            "the turn got the human's answer, not a timeout"
        );
    }

    /// An ALIAS is a name for a (provider, model, reasoning) triple. It
    /// must be RESOLVED, not forwarded.
    ///
    /// Both override paths used to assign `agent.model_override =
    /// alias_name` directly, so picking a valid alias in the models
    /// overlay sent its NAME to the provider as a model and came back
    ///   404 {"message":"Model not exist.","code":"model_not_found"}
    /// seconds into the turn, with nothing pointing at the override.
    #[tokio::test]
    async fn a_model_alias_is_resolved_not_forwarded() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        let agent = srv.agent_for("s1").expect("agent");
        let a = &agent;

        // 1. an alias resolves to its TARGET, both halves.
        a.set_model("fast").expect("a configured alias resolves");
        assert_eq!(
            a.overrides.read_ok().model.as_deref(),
            Some("alt-model"),
            "the alias NAME must never survive as the model"
        );
        assert_eq!(
            a.overrides.read_ok().provider.as_deref(),
            Some("alt-provider"),
            "resolving an alias must move the provider too"
        );

        // 2. a bare model name finds its provider.
        a.overrides.write_ok().provider = None;
        a.set_model("test-model").expect("a listed model resolves");
        assert_eq!(
            a.overrides.read_ok().provider.as_deref(),
            Some("test-provider")
        );

        // 3. an unresolvable name is still accepted — a provider need
        // not enumerate its models — but it must SAY it is guessing,
        // because that guess is what produces the 404 later.
        let msg = a
            .set_model("no-such-model-anywhere")
            .expect("guesses are allowed");
        assert!(
            msg.contains("UNVERIFIED"),
            "an unresolvable model must be reported as a guess, got: {msg}"
        );
    }

    /// The DEFERRED override path: pick a model for a session that has
    /// no cached agent yet, and the choice is parked in
    /// desktop_state.json until the session is first used.
    ///
    /// That path had its own copy of the raw assignment, so fixing the
    /// live handler alone left this one still forwarding alias names to
    /// the provider — and this is the path a fresh session takes, which
    /// is exactly when the 404 was seen.
    #[tokio::test]
    async fn a_deferred_alias_override_is_resolved_on_first_use() {
        let (_d, srv, home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();

        std::fs::write(
            home.path().join("desktop_state.json"),
            serde_json::to_string(&json!({
                "override:s1": { "model": "fast" }
            }))
            .unwrap(),
        )
        .unwrap();

        let agent = srv.agent_for("s1").expect("agent");
        let a = &agent;
        assert_eq!(
            a.overrides.read_ok().model.as_deref(),
            Some("alt-model"),
            "the deferred alias must be resolved, not parked verbatim"
        );
        assert_eq!(
            a.overrides.read_ok().provider.as_deref(),
            Some("alt-provider"),
            "resolving an alias must also switch the provider"
        );
        // The blob entry SURVIVES first use — only set_override's
        // clear removes it. A consume-and-delete here means a gateway
        // restart after the first message forgets the override.
        let state = load_desktop_state();
        assert_eq!(
            state["override:s1"]["model"], "fast",
            "the durable override must outlast first use, not be eaten"
        );
    }

    /// A LIVE override persists to the blob too, and a full clear
    /// erases it — the pair that "sessions forget their override across
    /// a gateway restart" (USER: "sometimes I had to set it again").
    #[tokio::test]
    async fn a_live_override_survives_a_restart_and_clear_erases_it() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        let agent = srv.agent_for("s1").expect("agent");
        srv.agents.lock_ok().insert("s1".into(), agent.clone());

        // LIVE branch (agent cached) must ALSO write the blob.
        let (code, _) = set_override(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(OverrideRequest {
                model: Some("fast".into()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        // The durable home is the store row now (R4.5) — one fact every
        // surface reads, not a desktop-private blob.
        let row = srv.store.lock_ok().override_get("s1").ok().flatten();
        assert!(
            row.as_ref()
                .is_some_and(|o| o.model.as_deref().is_some_and(|m| !m.is_empty())),
            "a live override must persist, not just mutate the cache: {row:?}"
        );

        // A full clear empties the row (and the in-memory override) —
        // the explicit empty-string clear is distinct from all-None.
        let (code, _) = set_override(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(OverrideRequest {
                model: Some(String::new()),
                provider: Some(String::new()),
                reasoning: Some(String::new()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let row = srv.store.lock_ok().override_get("s1").ok().flatten();
        let cleared = row
            .as_ref()
            .is_none_or(|o| o.provider.is_none() && o.model.is_none() && o.reasoning.is_none());
        assert!(
            cleared,
            "a cleared override must leave nothing behind: {row:?}"
        );
        assert!(agent.overrides.read_ok().model.is_none());
    }

    /// The LIVE override path, driven through the handler rather than
    /// through `set_model`, so that reverting the handler to a raw
    /// assignment fails here. Testing `set_model` alone left the call
    /// site uncovered — a mutation of it stayed green.
    #[tokio::test]
    async fn the_override_handler_resolves_aliases_and_rejects_junk() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();
        // Cache the agent, so the handler takes its LIVE branch.
        let agent = srv.agent_for("s1").expect("agent");
        srv.agents.lock_ok().insert("s1".into(), agent.clone());

        let (code, _) = set_override(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(OverrideRequest {
                model: Some("fast".into()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        {
            let a = &agent;
            assert_eq!(
                a.overrides.read_ok().model.as_deref(),
                Some("alt-model"),
                "the handler must resolve the alias, not store its name"
            );
            assert_eq!(
                a.overrides.read_ok().provider.as_deref(),
                Some("alt-provider")
            );
        }

        // An empty string CLEARS rather than pinning "".
        let (code, _) = set_override(
            State(srv.clone()),
            Path("s1".to_string()),
            Json(OverrideRequest {
                model: Some(String::new()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(agent.overrides.read_ok().model.is_none());

        // An unknown SESSION is a 404, not a panic.
        let (code, _) = set_override(
            State(srv.clone()),
            Path("nope".to_string()),
            Json(OverrideRequest::default()),
        )
        .await;
        assert_ne!(code, StatusCode::OK, "unknown session must not succeed");
    }
    /// Reading back the override must distinguish IN EFFECT from
    /// PENDING. Reporting a parked choice as if it were live is how a
    /// setting that never applied looks identical to one that did.
    #[tokio::test]
    async fn reading_back_an_override_separates_live_from_pending() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store.lock_ok().create_session("s1").unwrap();

        // No agent cached yet, and a parked choice in the store row —
        // the one durable home (R4.5).
        srv.store
            .lock_ok()
            .override_replace("s1", None, Some("fast"), None)
            .unwrap();
        let (code, Json(body)) = get_override(State(srv.clone()), Path("s1".to_string())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["live"], json!(false), "nothing is running yet");
        assert_eq!(body["pending"]["model"], json!("fast"));

        // Once the agent exists the answer is the REAL route, resolved.
        let agent = srv.agent_for("s1").expect("agent");
        srv.agents.lock_ok().insert("s1".into(), agent);
        let (_, Json(body)) = get_override(State(srv.clone()), Path("s1".to_string())).await;
        assert_eq!(body["live"], json!(true));
        assert_eq!(
            body["model"],
            json!("alt-model"),
            "must report the ALIAS TARGET — not the alias name, and not \
             the config default the session would otherwise use"
        );
        assert_eq!(body["provider"], json!("alt-provider"));
        assert_eq!(body["overridden"], json!(true));
    }
    /// Engine notes must come back from /history AT THE POINT THEY
    /// HAPPENED, not dumped at the end.
    ///
    /// Notes were stream events only, so a mechanism that acted while
    /// nobody was watching — a cron turn, a compaction during a
    /// disconnected session — was reported to no one and gone. The
    /// reopened session then looked like a conversation you had.
    #[tokio::test]
    async fn history_interleaves_engine_notes_where_they_happened() {
        let (_d, srv, _home, _env) = scratch_server();
        {
            let store = srv.store.lock_ok();
            store.create_session("s1").unwrap();
            store
                .append_message("s1", &veles_core::providers::ChatMessage::user("first"))
                .unwrap();
            store
                .record_note("s1", "schedule", "ran on a schedule", None, None)
                .unwrap();
            store
                .append_message(
                    "s1",
                    &veles_core::providers::ChatMessage::assistant("answer", None),
                )
                .unwrap();
            store
                .record_note("s1", "compaction", "compacted 80k → 2k", Some("why"), None)
                .unwrap();
            store
                .append_message("s1", &veles_core::providers::ChatMessage::user("second"))
                .unwrap();
        }

        let (code, Json(body)) = history(State(srv.clone()), Path("s1".to_string())).await;
        assert_eq!(code, StatusCode::OK);
        let msgs = body["messages"].as_array().expect("messages");

        let shape: Vec<String> = msgs
            .iter()
            .map(|m| {
                let role = m["role"].as_str().unwrap_or("?");
                if role == "note" {
                    format!("note:{}", m["kind"].as_str().unwrap_or("?"))
                } else {
                    role.to_string()
                }
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "user",
                "note:schedule",
                "assistant",
                "note:compaction",
                "user"
            ],
            "each note sits after the message it followed"
        );

        // The detail rides along, so the expander has something to show.
        let comp = msgs.iter().find(|m| m["kind"] == "compaction").unwrap();
        assert_eq!(comp["summary"], json!("compacted 80k → 2k"));
        assert_eq!(comp["detail"], json!("why"));
    }

    /// A note recorded before any message still has to appear.
    #[tokio::test]
    async fn a_note_before_the_first_message_is_not_lost() {
        let (_d, srv, _home, _env) = scratch_server();
        {
            let store = srv.store.lock_ok();
            store.create_session("s1").unwrap();
            store
                .record_note("s1", "memory", "12k injected", None, None)
                .unwrap();
            store
                .append_message("s1", &veles_core::providers::ChatMessage::user("hi"))
                .unwrap();
        }
        let (_, Json(body)) = history(State(srv.clone()), Path("s1".to_string())).await;
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], json!("note"), "{msgs:?}");
        assert_eq!(msgs[0]["kind"], json!("memory"));
        assert_eq!(msgs[1]["role"], json!("user"));
    }

    /// The reattach window PENDING named (2026-09-19): a note created
    /// AFTER a client synced, anchored at/before the client's message
    /// cursor. Its anchor is where it RENDERS, not when it was created,
    /// so replaying by `after_msg_id > since` skipped it forever.
    ///
    /// The note cursor closes it: the client resumes with `since_note`
    /// too, and the note's OWN id — not its anchor — decides whether
    /// this client has rendered it.
    #[tokio::test]
    async fn a_late_note_anchored_at_the_cursor_still_replays() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "late-note";
        let note_ids: Vec<i64>;
        let cursor: i64;
        {
            let store = srv.store.lock_ok();
            store.create_session(sid).unwrap();
            store
                .append_message(sid, &veles_core::providers::ChatMessage::user("hello"))
                .unwrap();
            // The client syncs NOW: message cursor = 1, no notes yet.
            cursor = store.last_msg_id(sid).unwrap();
            // A note arrives afterwards, anchored at the newest message
            // that existed when it was recorded (msg 1).
            note_ids = vec![store
                .record_note(sid, "guard", "iteration cap hit", None, None)
                .unwrap()];
        }

        // The client resumes with BOTH cursors: its message cursor (=
        // 1) and its note cursor (0 — it had seen no note before the
        // drop). The note's id beats its anchor: it replays even
        // though every message sits at/below the message cursor.
        let rows = history_rows(&srv.store.lock_ok(), &srv.cfg.read_ok(), sid, cursor, 0).unwrap();
        let replayed: Vec<&Value> = rows.iter().filter(|r| r["role"] == "note").collect();
        assert_eq!(replayed.len(), 1, "the late note replays: {rows:?}");
        assert_eq!(replayed[0]["note_id"], json!(note_ids[0]));
        assert_eq!(replayed[0]["kind"], json!("guard"));

        // And a client that HAS seen it (note cursor advanced) gets
        // nothing — the cursor is a resume point, not a drumbeat.
        let after = history_rows(
            &srv.store.lock_ok(),
            &srv.cfg.read_ok(),
            sid,
            cursor,
            note_ids[0],
        )
        .unwrap();
        assert!(
            after.iter().all(|r| r["role"] != "note"),
            "a caught-up note cursor replays nothing: {after:?}"
        );
    }
    /// `usage_totals` returns (CALLS, prompt, completion). Both desktop
    /// readers destructured it as (prompt, completion, messages), so
    /// every field was shifted by one and the status bar reported the
    /// CALL COUNT as the prompt total.
    ///
    /// USER: "the token counts are non-annotated and/or seem wrong".
    /// The store was right; the readers were not.
    /// USER: "where/how do you see/edit the tcl script when set to
    /// script?" — the blueprint offered a `script` mode with nothing
    /// behind it. The editor is only useful if the endpoint hands back
    /// a runnable starting point rather than an empty box.
    #[tokio::test]
    async fn failover_script_offers_a_template_before_one_exists() {
        let (_d, srv, _home, _env) = scratch_server();
        let Json(body) =
            failover_script(State(srv.clone()), Query(ScriptQuery { name: None })).await;
        assert_eq!(body["exists"], false, "nothing written yet");
        let src = body["source"].as_str().unwrap();
        assert!(
            src.contains("::veles::hook failover"),
            "the template must actually register the hook, or saving it \
             produces a plugin that never fires: {src}"
        );
        assert!(
            src.contains("from_provider") && src.contains("action"),
            "the template must document the hook's real contract: {src}"
        );
        assert_eq!(body["hook"], "failover");
    }

    /// The wake-ownership split: the desktop owns the sessions IT holds
    /// (a wake for one should run against the desktop's agent), and it
    /// must NEVER claim a chat-backed session — the gateway (same
    /// process) owns those, and their wake must deliver into the
    /// chat. The split is routable even though the two maps never see
    /// each other: the gateway publishes its core process-wide, and
    /// the desktop consults it first.
    #[tokio::test]
    async fn the_desktop_owns_only_sessions_it_holds_and_the_gateway_does_not() {
        let (_d, srv, _home, _env) = scratch_server();
        {
            let store = srv.store.lock_ok();
            store.create_session("wake-owned-sess").unwrap();
            store.create_session("wake-foreign-sess").unwrap();
        }
        // Not held yet → nobody's.
        assert!(
            !srv.owns_session("wake-owned-sess"),
            "a session with no agent cached is not the desktop's to wake"
        );
        // Hold it (the agent_for path is the same one a turn would
        // use).
        let _ = srv
            .agent_for("wake-owned-sess")
            .expect("agent builds for a scratch-store session");
        assert!(
            srv.owns_session("wake-owned-sess"),
            "a held session is the desktop's to wake"
        );
        // A session the desktop does NOT hold stays out.
        assert!(
            !srv.owns_session("wake-foreign-sess"),
            "an unheld session must not be claimed"
        );

        // Chat-backed: the gateway in-process claims it, so the
        // desktop must stand down even if it holds the agent (a desktop
        // window may run a turn on a chat-backed session).
        let _ = srv
            .agent_for("wake-foreign-sess")
            .expect("agent builds for a scratch-store session");
        {
            // Park a gateway core that owns this session, the way the
            // real gateway does at boot.
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(std::sync::Mutex::new(
                veles_core::store::Store::open(&dir.path().join("gateway.db")).unwrap(),
            ));
            let _ = store.lock().unwrap().create_session("wake-foreign-sess");
            let core = crate::gateway::GatewayCore::new_for_test(store);
            core.mark_chat_backed("wake-foreign-sess", "irc", "#chan");
            crate::gateway::set_global_core(core);
        }
        assert!(
            !srv.owns_session("wake-foreign-sess"),
            "a chat-backed session belongs to the gateway, not the desktop"
        );
    }

    /// A window showing a session IS the desktop holding it.
    ///
    /// `holds_for_mail` already says so — a session a window is watching
    /// counts as held, which is the whole point of `watch()` ("it runs
    /// when the session is next opened"). `owns_session` said otherwise:
    /// it asked the cached-agent map alone, and that map is EMPTY for a
    /// session a window opened but never POSTed to — every freshly opened
    /// transcript, and every session after `reload_config` cleared the
    /// cache. A completion in one of those was then claimed by NOBODY:
    /// the gateway answers false (not chat-backed), the desktop answered
    /// false (no cached agent), so `run_wake_loop` dropped the event on
    /// `if !owner.own_sid(...)`, and the monitor only fired once the
    /// human typed something and pushed the parked event through a real
    /// turn's drain. That is exactly the reported symptom.
    ///
    /// Mutation: drop the watcher arm from `owns_session` and this fails.
    #[tokio::test]
    async fn a_watched_session_is_the_desktops_to_wake_without_an_agent() {
        let (_d, srv, _home, _env) = scratch_server();
        srv.store
            .lock_ok()
            .create_session("wake-watched-sess")
            .unwrap();
        // A window opens the session: the event stream holds a watch,
        // and no agent is built (opening a transcript runs no turn).
        let watch = srv.watch("wake-watched-sess");
        assert!(
            srv.agents.lock_ok().is_empty(),
            "opening a transcript must not build an agent, or this test \
             would be proving the cached-agent arm instead"
        );
        assert!(
            srv.owns_session("wake-watched-sess"),
            "a session a window is showing has an owner: the desktop"
        );
        // The config reload that evicts the agent cache must not orphan
        // a session a window still has open.
        srv.agents.lock_ok().clear();
        assert!(
            srv.owns_session("wake-watched-sess"),
            "evicting the agent cache must not make a watched session \
             ownerless"
        );
        drop(watch);
        assert!(
            !srv.owns_session("wake-watched-sess"),
            "the last window closed: nobody holds it again"
        );
    }

    /// A monitor finishing at the old turn's teardown still starts the next
    /// turn without any user message.
    ///
    /// The first completion bell is expected to stand down: the old turn
    /// is live, so its own drain normally delivers. This test removes
    /// that later drain deliberately. `RunRegistration` must ring again
    /// after clearing the live row, and the surface must carry the wake
    /// through its still-held turn lock. This is the race that otherwise
    /// leaves the monitor result parked until somebody types.
    #[tokio::test]
    async fn a_monitor_at_turn_teardown_rings_again_and_wakes_the_idle_session() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = format!("wake-teardown-monitor-{}", uuid::Uuid::new_v4());
        srv.store.lock_ok().create_session(&sid).unwrap();
        let _watch = srv.watch(&sid);
        let reply = r#"data: {"choices":[{"delta":{"content":"teardown wake"}}]}

data: [DONE]

"#;
        let (url, requests) = veles_core::providers::tests::sse_server_seq_capture(vec![
            reply.to_string(),
            reply.to_string(),
        ])
        .await;
        let mut cfg = (**srv.cfg.read_ok()).clone();
        cfg.custom_providers[0].base_url = url;
        *srv.cfg.write_ok() = Arc::new(cfg);
        let agent = srv.agent_for(&sid).expect("desktop agent");
        let lock = srv.turn_lock(&sid);
        let held = lock.lock().await;
        // Simulate the narrow teardown window using the store's public
        // registration: the original wake must see this and stand down.
        agent
            .store
            .lock_ok()
            .run_start(&sid, std::process::id(), "test", "desktop")
            .unwrap();
        let owner: Arc<dyn veles_core::session_wake::WakeOwner> =
            Arc::new(DesktopWakeOwner { srv: srv.clone() });
        let loop_task = veles_core::rt::spawn(veles_core::session_wake::run_wake_loop(owner));
        let id = veles_core::monitor::start(
            veles_core::monitor::Predicate::Shell("exit 0".into()),
            Some(30),
            &sid,
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !veles_core::delegate::has_background_events(&sid) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("monitor never queued its completion");
        // Give the original completion bell a chance to observe the live
        // row. It must not call the provider while the old turn is live.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            requests.lock().unwrap().is_empty(),
            "a busy session started a second provider turn"
        );
        // The engine has now ended, but the surface still owns the old
        // lock. Clear exactly the live-row half; the engine's pending
        // signal then follows the same handoff as RunRegistration::drop.
        agent.store.lock_ok().run_end(&sid).unwrap();
        veles_core::session_tap::publish(
            &sid,
            "background-wake-pending",
            json!({ "session_id": sid }),
        );
        // Release the old surface turn only after the retry has polled.
        tokio::task::yield_now().await;
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while requests.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "monitor {id} remained stranded across the old turn's teardown; pending={}",
                veles_core::delegate::has_background_events(&sid)
            )
        });
        loop_task.abort();
        let request = requests.lock().unwrap()[0].clone();
        assert!(request.contains("[MONITOR FIRED]"), "{request}");
    }

    /// A REAL monitor wakes an idle watched desktop session and runs a turn.
    ///
    /// This deliberately drives the whole chain: detached monitor poll,
    /// delegate queue, session bus, wake ownership, per-session lock,
    /// agent construction, and provider request. Calling
    /// `desktop_wake_one` or publishing a synthetic bus event would prove
    /// less than the operator's unattended-wait contract requires.
    #[tokio::test]
    async fn a_real_monitor_starts_a_turn_in_an_idle_watched_session() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = format!("wake-real-monitor-{}", uuid::Uuid::new_v4());
        srv.store.lock_ok().create_session(&sid).unwrap();
        let _watch = srv.watch(&sid);
        let reply = r#"data: {"choices":[{"delta":{"content":"monitor woke me"}}]}

data: [DONE]

"#;
        let (url, requests) = veles_core::providers::tests::sse_server_seq_capture(vec![
            reply.to_string(),
            reply.to_string(),
        ])
        .await;
        let mut cfg = (**srv.cfg.read_ok()).clone();
        cfg.custom_providers[0].base_url = url;
        *srv.cfg.write_ok() = Arc::new(cfg);
        let owner: Arc<dyn veles_core::session_wake::WakeOwner> =
            Arc::new(DesktopWakeOwner { srv: srv.clone() });
        let loop_task = veles_core::rt::spawn(veles_core::session_wake::run_wake_loop(owner));
        let id = veles_core::monitor::start(
            veles_core::monitor::Predicate::Shell("exit 0".into()),
            Some(30),
            &sid,
        );
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while requests.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "monitor {id} finished without waking the idle desktop session; pending={}",
                veles_core::delegate::has_background_events(&sid)
            )
        });
        loop_task.abort();
        let request = requests.lock().unwrap()[0].clone();
        assert!(request.contains("[MONITOR FIRED]"), "{request}");
        assert!(request.contains("condition met"), "{request}");
    }

    /// A save has to survive a restart, which means the manifest has to
    /// exist: plugins are DISCOVERED by plugin.toml, so a lone
    /// plugin.tcl works until the process restarts and then silently
    /// stops working.
    #[tokio::test]
    async fn saving_the_failover_script_writes_a_discoverable_plugin() {
        let (_d, srv, _home, _env) = scratch_server();
        let script = "namespace eval failover {}\nproc failover::handle {} { return \"\" }\n";
        let (code, Json(body)) =
            save_failover_script(State(srv.clone()), Json(json!({ "source": script }))).await;
        assert_eq!(code, StatusCode::OK, "{body}");

        let dir = failover_plugin_dir();
        let written = std::fs::read_to_string(dir.join("plugin.tcl")).expect("plugin.tcl");
        assert_eq!(written, script);
        let manifest = std::fs::read_to_string(dir.join("plugin.toml")).expect("plugin.toml");
        assert!(
            manifest.contains("provides_hooks = [\"failover\"]"),
            "the manifest must declare the hook or discover() skips it: {manifest}"
        );
        // The staging copy must not be left behind — it would be
        // discovered as neither plugin nor backup.
        assert!(!dir.join("plugin.tcl.new").exists());

        // Reading it back returns what was saved, not the template.
        let Json(after) =
            failover_script(State(srv.clone()), Query(ScriptQuery { name: None })).await;
        assert_eq!(after["exists"], true);
        assert_eq!(after["source"].as_str().unwrap(), script);
    }

    /// Saving twice keeps the previous script. An editor with no undo
    /// that overwrites a working hook is worse than no editor.
    #[tokio::test]
    async fn saving_again_keeps_the_previous_script() {
        let (_d, srv, _home, _env) = scratch_server();
        let first = "proc failover::handle {} { return \"first\" }\n";
        let second = "proc failover::handle {} { return \"second\" }\n";
        let _ = save_failover_script(State(srv.clone()), Json(json!({ "source": first }))).await;
        let _ = save_failover_script(State(srv.clone()), Json(json!({ "source": second }))).await;

        let dir = failover_plugin_dir();
        assert_eq!(
            std::fs::read_to_string(dir.join("plugin.tcl")).unwrap(),
            second
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("plugin.tcl.bak")).unwrap(),
            first
        );
    }

    #[tokio::test]
    async fn an_empty_failover_script_is_refused() {
        let (_d, srv, _home, _env) = scratch_server();
        let (code, _) =
            save_failover_script(State(srv.clone()), Json(json!({ "source": "   \n" }))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(
            !failover_plugin_dir().join("plugin.tcl").exists(),
            "a refused save must not leave a file behind"
        );
    }

    /// The editor and the loader must agree on WHERE the plugin lives.
    ///
    /// `failover_plugin_dir()` used to read `veles_home()` (the DATA
    /// root) while the loader sources plugins from
    /// `plugins::plugins_dir()` (the CONFIG root) — the same XDG-split
    /// divergence `9946f92` swept out of the read-side frontends, and
    /// the tests around this one could not see it because
    /// `scratch_home()` FORCES one directory, collapsing the two roots.
    /// This test holds them apart: a save through the real handler must
    /// land where `plugins::discover_all` — the loader's own discovery
    /// — actually looks, and must not scatter anything into the data
    /// root.
    #[tokio::test]
    async fn the_failover_editor_saves_where_the_loader_loads() {
        let (cfg_root, data_root, _env) = veles_core::testenv::scratch_xdg();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let srv = Arc::new(DesktopServer::new(
            Arc::new(VelesConfig::default()),
            store,
            HookBus::new(None),
        ));

        let script = "namespace eval failover {}\nproc failover::handle {} { return \"\" }\n";
        let (code, Json(body)) =
            save_failover_script(State(srv.clone()), Json(json!({ "source": script }))).await;
        assert_eq!(code, StatusCode::OK, "{body}");

        // The LOADER's discovery finds it: this is
        // `plugins::plugins_dir()` — config root — the exact call the
        // boot-time plugin loader makes.
        let discovered = veles_core::plugins::discover_all().expect("discover");
        assert!(
            discovered.iter().any(|p| p.manifest.name == "failover"),
            "the saved plugin must be discovered where the loader loads \
             from (config root {}): {:?}",
            veles_core::plugins::plugins_dir().display(),
            discovered
                .iter()
                .map(|p| p.manifest.name.clone())
                .collect::<Vec<_>>()
        );

        // And the DATA root stays clean: no shadow copy, no half-built
        // staging file, nothing for a future reader to mistake for the
        // plugin's home.
        let stray = data_root.path().join("veles/plugins");
        assert!(
            !stray.exists(),
            "nothing may be written under the data root's plugins/: {} exists",
            stray.display()
        );
        // The config root is where it all landed (sanity: the dirs the
        // override picked are the ones the handler used).
        assert!(cfg_root
            .path()
            .join("veles/plugins/failover/plugin.tcl")
            .exists());
    }

    /// A plugin sitting in the directory is not a plugin in the process:
    /// a non-empty `enabled` list is an allowlist. The editor has to say
    /// so, or you edit dead code and wonder why nothing fires.
    #[tokio::test]
    async fn the_editor_says_when_the_plugin_would_not_load() {
        let (_d, srv, _home, _env) = scratch_server();
        let Json(body) =
            failover_script(State(srv.clone()), Query(ScriptQuery { name: None })).await;
        assert_eq!(
            body["loaded"], true,
            "an empty enabled list loads everything discovered"
        );

        let mut cfg = (*srv.cfg()).clone();
        cfg.plugins.disabled = vec!["failover".into()];
        let srv2 = Arc::new(DesktopServer::new(
            Arc::new(cfg),
            srv.store.clone(),
            srv.hooks.clone(),
        ));
        let Json(body) = failover_script(State(srv2), Query(ScriptQuery { name: None })).await;
        assert_eq!(body["loaded"], false);
        assert!(
            body["note"].as_str().unwrap().contains("fire nothing"),
            "and it has to say what that means: {}",
            body["note"]
        );
    }

    #[tokio::test]
    async fn usage_reports_tokens_not_the_call_count() {
        let (_d, srv, _home, _env) = scratch_server();
        {
            let store = srv.store.lock_ok();
            store.create_session("s1").unwrap();
            // Three calls, deliberately distinct magnitudes so a shifted
            // field cannot coincidentally match.
            store
                .record_usage(
                    "s1",
                    "p",
                    "m",
                    veles_core::store::CallTokens {
                        prompt_tokens: 1000,
                        completion_tokens: 10,
                        cached_tokens: 900,
                        cache_write_tokens: 0,
                        cache_reported: true,
                        reasoning_tokens: None,
                    },
                )
                .unwrap();
            store
                .record_usage(
                    "s1",
                    "p",
                    "m",
                    veles_core::store::CallTokens {
                        prompt_tokens: 2000,
                        completion_tokens: 20,
                        cached_tokens: 1800,
                        cache_write_tokens: 0,
                        cache_reported: true,
                        reasoning_tokens: None,
                    },
                )
                .unwrap();
            store
                .record_usage(
                    "s1",
                    "p",
                    "m",
                    veles_core::store::CallTokens {
                        prompt_tokens: 3000,
                        completion_tokens: 30,
                        cached_tokens: 2700,
                        cache_write_tokens: 0,
                        cache_reported: true,
                        reasoning_tokens: None,
                    },
                )
                .unwrap();
        }

        let Json(body) = usage(
            State(srv.clone()),
            Query(UsageQuery {
                session: Some("s1".to_string()),
            }),
        )
        .await;

        assert_eq!(
            body["prompt_tokens"],
            json!(6000),
            "prompt is the SUM: {body}"
        );
        assert_eq!(body["completion_tokens"], json!(60), "{body}");
        assert_eq!(
            body["calls"],
            json!(3),
            "the call count is its own field: {body}"
        );
        let sess = &body["session"];
        assert_eq!(sess["prompt_tokens"], json!(6000), "{body}");
        assert_eq!(sess["completion_tokens"], json!(60), "{body}");
        assert_eq!(sess["calls"], json!(3), "{body}");

        // The cache share rides alongside the token counts now.
        assert_eq!(body["cached_tokens"], json!(5400), "{body}");
        assert_eq!(sess["cached_tokens"], json!(5400), "{body}");

        // The shifted reading would have put 3 (the call count) here.
        assert_ne!(
            body["prompt_tokens"],
            json!(3),
            "the call count must never be reported as tokens"
        );
    }
    // ── IRC ──────────────────────────────────────────────────────────

    fn irc_line(srv: &Arc<DesktopServer>, buffer: &str, text: &str) -> i64 {
        srv.store
            .lock_ok()
            .irc_append(
                "irc.example",
                "bot",
                buffer,
                "2026-08-25T00:00:00Z",
                "privmsg",
                "wrath",
                None,
                text,
                false,
                0,
                false,
            )
            .expect("append")
    }

    /// The desktop kept THREE hand-maintained copies of the command
    /// table (`chat.tcl`'s switch and its help literal,
    /// `complete.tcl`'s "mirror of chat.tcl", `palette.tcl`'s rows) and
    /// they had drifted: they listed commands that do not exist and
    /// omitted `/queue`, `/context`, `/secret`, `/guards`, `/sessions`
    /// and `/cron`. One table, served — with each row's capability, so
    /// a surface can mark the admin rows instead of pretending
    /// everything is available.
    #[tokio::test]
    async fn the_command_table_is_served_not_copied() {
        let Json(v) = commands_view().await;
        let rows = v["commands"].as_array().expect("commands");
        assert!(rows.len() > 30, "the whole table, not a sample");
        let by_name = |n: &str| {
            rows.iter()
                .find(|r| r["name"].as_str() == Some(n))
                .cloned()
                .unwrap_or(Value::Null)
        };
        // The six the hand-copies omitted.
        for missing in [
            "/queue",
            "/context",
            "/secret",
            "/guards",
            "/sessions",
            "/crons",
        ] {
            assert!(
                !by_name(missing).is_null(),
                "{missing} must be in the served table — the desktop copies dropped it"
            );
        }
        // Every row carries a usage string and a description.
        for r in rows {
            assert!(
                r["usage"].as_str().is_some_and(|u| u.starts_with('/')),
                "a row without a usage string: {r}"
            );
            assert!(r["desc"].as_str().is_some_and(|d| !d.is_empty()), "{r}");
        }
        // And the capability, where the engine has one — that is what
        // lets discovery stop lying to a non-admin.
        assert_eq!(
            by_name("/secret")["cap"].as_str(),
            Some("cmd-admin"),
            "the admin rows are marked"
        );
        assert_eq!(
            by_name("/status")["cap"].as_str(),
            Some("cmd-status"),
            "and so is the open floor"
        );
    }

    /// `POST /api/command` — the desktop runs the ENGINE's shared
    /// commands instead of reimplementing them.
    ///
    /// It was the only surface that never called `commands::dispatch`,
    /// which is why its `/help` could advertise `/queue`, `/context`
    /// and `/limits` and then answer "unknown command: /queue".
    #[tokio::test]
    async fn a_shared_command_runs_for_a_desktop_session() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "cmd-session";
        srv.store.lock_ok().create_session(sid).expect("session");

        let run = |line: &str| {
            let srv = srv.clone();
            let line = line.to_string();
            async move {
                let (code, Json(v)) = command_run(
                    State(srv),
                    Json(RunCommandBody {
                        session_id: sid.to_string(),
                        command: line,
                    }),
                )
                .await;
                (code, v)
            }
        };

        let (code, v) = run("/limits").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["handled"].as_bool(), Some(true), "{v}");
        assert!(
            v["text"].as_str().unwrap_or("").contains("150k"),
            "the engine's own answer, not a Tcl copy of it: {v}"
        );

        // A name the engine does not know is `handled: false`, NOT an
        // error — the caller falls through to custom commands and only
        // then says "unknown command".
        let (code, v) = run("/frobnicate").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["handled"].as_bool(), Some(false), "{v}");

        // `/sandbox` is async; the dispatcher blocks on a fresh runtime
        // for it, which cannot happen inside this one — so it is
        // awaited here instead. If that special case were dropped this
        // call would panic rather than answer.
        let (code, v) = run("/sandbox list").await;
        assert_eq!(code, StatusCode::OK);
        assert!(
            v["text"]
                .as_str()
                .unwrap_or("")
                .contains("admin default zone"),
            "{v}"
        );

        // Not a command at all, and an unknown session, each say so.
        let (code, _) = run("limits").await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        let (code, _) = command_run(
            State(srv.clone()),
            Json(RunCommandBody {
                session_id: "nope".into(),
                command: "/limits".into(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    /// `/api/acl/explain` — the desktop's answer to "why does this
    /// person have this role", which specificity precedence makes a
    /// question about a comparison rather than a line number.
    #[tokio::test]
    async fn the_acl_overlay_can_ask_why() {
        let (_d, srv, _home, _env) = scratch_server();
        // A CONFIGURED ACL, written as a real file in the scratch home:
        // with no `[acl]` at all the ACL is dormant and resolves every
        // sender as owner, so an explain test against a default config
        // would be measuring the legacy posture. The file also pins
        // which config the handler reads on a machine that has one of
        // its own.
        let dir = veles_core::config::config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        //
        // The two rules go in the FILE rather than in the runtime
        // store: `acl::runtime_pairing_store` is a process-global
        // OnceLock that the first test to set it owns, so a grant
        // written to this server's store is not necessarily the one
        // resolution reads. Config entries have no such ambiguity —
        // and the property under test is the comparison, not where a
        // rule was stored.
        //
        // Declared in the WRONG order on purpose: under the old
        // first-match-in-config-order resolution the broad rule would
        // have won and jimmy would have been a user.
        std::fs::write(
            dir.join("config.toml"),
            "[acl]\nowner = [\"irc:me\"]\n\n\
             [[acl.entries]]\nidentity = \"*!*@*.example\"\nrole = \"user\"\n\n\
             [[acl.entries]]\nidentity = \"jimmy!asshole@444.example\"\nrole = \"guest\"\n",
        )
        .expect("write config");
        veles_core::acl::invalidate_live_config();

        let Json(v) = acl_explain(
            State(srv.clone()),
            Query(AclExplainQuery {
                identity: "jimmy!asshole@444.example".into(),
                network: None,
                channel: None,
                platform: None,
            }),
        )
        .await;
        assert_eq!(
            v["role"].as_str(),
            Some("guest"),
            "the narrowest rule wins regardless of which was stored first: {v}"
        );
        let rules = v["rules"].as_array().expect("rules");
        assert!(
            rules.len() >= 2,
            "the runners-up are reported, not just the winner: {v}"
        );
        assert_eq!(rules[0]["winner"].as_bool(), Some(true));
        assert!(
            rules
                .iter()
                .any(|r| r["identity"].as_str() == Some("*!*@*.example")),
            "the broad rule it beat is named: {v}"
        );
        assert!(
            v["max_context_tokens"].as_u64() == Some(150_000),
            "a guest's ceiling rides along, because it is the other thing \
             an operator is about to be asked about: {v}"
        );
    }

    /// `/api/acl` grew the ceilings, and `/api/acl/limits` sets one
    /// live — the same numbers `/limits` answers with on every other
    /// surface.
    #[tokio::test]
    async fn the_acl_view_carries_the_ceilings_and_can_set_them() {
        let (_d, srv, _home, _env) = scratch_server();
        let ceiling = |v: &Value, role: &str| -> u64 {
            v["limits"]
                .as_array()
                .expect("limits")
                .iter()
                .find(|r| r["role"].as_str() == Some(role))
                .and_then(|r| r["max_context_tokens"].as_u64())
                .unwrap_or(u64::MAX)
        };
        let Json(v) = acl_view(State(srv.clone())).await;
        assert_eq!(ceiling(&v, "guest"), 150_000, "{v}");
        assert_eq!(ceiling(&v, "user"), 250_000, "{v}");
        assert_eq!(ceiling(&v, "admin"), 0, "0 = unlimited: {v}");

        let (code, Json(out)) = acl_limit_set(
            State(srv.clone()),
            Json(AclLimitBody {
                which: "guest".into(),
                budget: None,
                max_context_tokens: Some(90_000),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        let Json(v) = acl_view(State(srv.clone())).await;
        assert_eq!(
            ceiling(&v, "guest"),
            90_000,
            "the runtime override is what the view now reports: {v}"
        );

        // A name that is neither a role nor `default` is refused, not
        // stored as a row nothing will ever read.
        let (code, _) = acl_limit_set(
            State(srv.clone()),
            Json(AclLimitBody {
                which: "wizard".into(),
                budget: None,
                max_context_tokens: Some(1),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

        // Clearing hands it back to config.
        let _ = acl_limit_set(
            State(srv.clone()),
            Json(AclLimitBody {
                which: "guest".into(),
                budget: None,
                max_context_tokens: None,
            }),
        )
        .await;
        let Json(v) = acl_view(State(srv.clone())).await;
        assert_eq!(ceiling(&v, "guest"), 150_000, "{v}");

        // The spend budgets ride the same endpoint. `budget: None`
        // above means the context ceiling, so a client written before
        // they existed keeps working — and naming one must move THAT
        // budget and leave the context ceiling alone.
        let (code, Json(out)) = acl_limit_set(
            State(srv.clone()),
            Json(AclLimitBody {
                which: "guest".into(),
                budget: Some("tokens-per-day".into()),
                max_context_tokens: Some(40_000),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        let Json(v) = acl_view(State(srv.clone())).await;
        let row = v["limits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["role"] == "guest")
            .unwrap()
            .clone();
        assert_eq!(row["max_tokens_per_day"], 40_000, "{v}");
        assert_eq!(
            ceiling(&v, "guest"),
            150_000,
            "setting tokens/day must not touch the context ceiling: {v}"
        );
        assert!(
            v["max_concurrent_turns"].is_number(),
            "the pane needs the concurrency cap too: {v}"
        );
        assert!(
            v["runtime_budgets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["budget"] == "tokens-per-day" && r["which"] == "guest"),
            "the override is visible as an override: {v}"
        );

        // An unknown budget name is refused rather than silently
        // treated as the context ceiling.
        let (code, _) = acl_limit_set(
            State(srv.clone()),
            Json(AclLimitBody {
                which: "guest".into(),
                budget: Some("vibes".into()),
                max_context_tokens: Some(1),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// `/api/sandboxes` — the desktop had no `user_envs` reference at
    /// all, so the one surface with room for a table of containers and
    /// who lands in which could not see them.
    #[tokio::test]
    async fn the_sandbox_endpoints_declare_assign_and_report() {
        let (_d, srv, _home, _env) = scratch_server();

        let (code, Json(out)) = sandbox_create(
            State(srv.clone()),
            Json(SandboxDefBody {
                name: "quarantine".into(),
                image: Some("localhost/veles-sandbox:test".into()),
                memory_mb: Some(512),
                network: Some(false),
                note: Some("guests only".into()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");

        let (code, Json(out)) = sandbox_assign(
            State(srv.clone()),
            Json(SandboxBindBody {
                sandbox: "quarantine".into(),
                identity: None,
                network: Some("libera".into()),
                // A blank field from a form is "no selector", not "match
                // the empty string" — the 422 shape the ACL UI already
                // paid for once.
                channel: Some("   ".into()),
                platform: None,
                role: Some("guest".into()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");

        let Json(v) = sandboxes_view(State(srv.clone())).await;
        let defs = v["defs"].as_array().expect("defs");
        let d = defs
            .iter()
            .find(|d| d["name"].as_str() == Some("quarantine"))
            .expect("the declaration is listed");
        assert_eq!(d["memory_mb"].as_u64(), Some(512));
        assert_eq!(d["note"].as_str(), Some("guests only"));
        assert_eq!(d["created_by"].as_str(), Some("desktop"));
        let binds = v["binds"].as_array().expect("binds");
        let b = binds
            .iter()
            .find(|b| b["sandbox"].as_str() == Some("quarantine"))
            .expect("the assignment is listed");
        assert_eq!(b["role"].as_str(), Some("guest"));
        assert_eq!(b["network"].as_str(), Some("libera"));
        assert!(
            b["channel"].is_null(),
            "a blank form field must not become a selector: {b}"
        );
        assert_eq!(
            v["local_default_zone"].as_str(),
            Some("host"),
            "the desktop's OWN zone, which is the pane's other half: {v}"
        );
        assert!(
            v["admin_default_zone"].as_str().is_some(),
            "the posture an admin needs before opting out: {v}"
        );

        // A bind with no selectors at all would capture everyone.
        let (code, _) = sandbox_assign(
            State(srv.clone()),
            Json(SandboxBindBody {
                sandbox: "quarantine".into(),
                identity: None,
                network: None,
                channel: None,
                platform: None,
                role: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

        // Unassign takes the same selectors; drop forgets the
        // declaration without touching the workspace.
        let Json(out) = sandbox_unassign(
            State(srv.clone()),
            Json(SandboxBindBody {
                sandbox: "quarantine".into(),
                identity: None,
                network: Some("libera".into()),
                channel: None,
                platform: None,
                role: Some("guest".into()),
            }),
        )
        .await;
        assert_eq!(out["removed"].as_bool(), Some(true), "{out}");
        let Json(out) = sandbox_drop(
            State(srv.clone()),
            Json(SandboxNameBody {
                name: "quarantine".into(),
                delete_work: false,
            }),
        )
        .await;
        assert_eq!(out["dropped"].as_bool(), Some(true), "{out}");
        let Json(v) = sandboxes_view(State(srv.clone())).await;
        assert!(
            !v["defs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["name"].as_str() == Some("quarantine")),
            "{v}"
        );
    }

    #[tokio::test]
    async fn irc_state_lists_buffers_with_history_not_just_live_channels() {
        let (_d, srv, _home, _env) = scratch_server();
        irc_line(&srv, "#dev", "a channel we since left");

        let Json(v) = irc_state(State(srv.clone())).await;
        let nets = v["networks"].as_array().expect("networks");
        // The tap has no live connection in a test, so the network list
        // comes from whatever state the process has seen. What must
        // hold regardless is that latest_id tracks the store — it is
        // what a fresh client uses to start live instead of replaying
        // everything ever said.
        assert_eq!(
            v["latest_id"].as_i64().unwrap(),
            srv.store.lock_ok().irc_latest_id().unwrap(),
            "latest_id is where 'now' is"
        );
        let _ = nets;
    }

    #[tokio::test]
    async fn irc_backlog_pages_backwards_through_one_buffer() {
        let (_d, srv, _home, _env) = scratch_server();
        let mut ids = Vec::new();
        for i in 0..6 {
            ids.push(irc_line(&srv, "#veles", &format!("line {i}")));
        }
        irc_line(&srv, "#other", "not this one");

        let Json(v) = irc_backlog(
            State(srv.clone()),
            Query(IrcBacklogQuery {
                network: "irc.example".into(),
                presence: "bot".into(),
                buffer: "#veles".into(),
                before: None,
                limit: 2,
            }),
        )
        .await;
        let lines = v["lines"].as_array().expect("lines");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["text"], "line 4", "the newest page, oldest first");
        assert_eq!(lines[1]["text"], "line 5");

        // Scrolling up from line 4.
        let Json(v2) = irc_backlog(
            State(srv.clone()),
            Query(IrcBacklogQuery {
                network: "irc.example".into(),
                presence: "bot".into(),
                buffer: "#veles".into(),
                before: Some(ids[4]),
                limit: 2,
            }),
        )
        .await;
        let older = v2["lines"].as_array().unwrap();
        assert_eq!(older[0]["text"], "line 2");
        assert_eq!(older[1]["text"], "line 3");
    }

    #[tokio::test]
    async fn irc_backlog_does_not_bleed_between_buffers_or_presences() {
        let (_d, srv, _home, _env) = scratch_server();
        irc_line(&srv, "#veles", "bot side");
        srv.store
            .lock_ok()
            .irc_append(
                "irc.example",
                "admin",
                "#veles",
                "2026-08-25T00:00:00Z",
                "privmsg",
                "wrath",
                None,
                "admin side",
                true,
                0,
                false,
            )
            .unwrap();

        let Json(v) = irc_backlog(
            State(srv.clone()),
            Query(IrcBacklogQuery {
                network: "irc.example".into(),
                presence: "bot".into(),
                buffer: "#veles".into(),
                before: None,
                limit: 50,
            }),
        )
        .await;
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "the admin's #veles is not the bot's");
        assert_eq!(lines[0]["text"], "bot side");
    }

    #[tokio::test]
    async fn a_backlog_limit_is_clamped_not_trusted() {
        // The limit comes off a query string. Zero would return nothing
        // and look like an empty channel; a huge one would let a client
        // pull the whole table into memory in one request.
        let (_d, srv, _home, _env) = scratch_server();
        for i in 0..5 {
            irc_line(&srv, "#veles", &format!("line {i}"));
        }
        let Json(v) = irc_backlog(
            State(srv.clone()),
            Query(IrcBacklogQuery {
                network: "irc.example".into(),
                presence: "bot".into(),
                buffer: "#veles".into(),
                before: None,
                limit: 0,
            }),
        )
        .await;
        assert_eq!(
            v["lines"].as_array().unwrap().len(),
            1,
            "a zero limit is clamped to one line, not to none"
        );
    }

    // ── fs_file: the editor's full-content read ─────────────────────
    //
    // The Dev pane opened files through fs_read, a preview HEAD capped
    // at 200 lines, and inherited its truncation all the way into the
    // LSP's didOpen — "missing braces" diagnostics on files cut off
    // mid-block. fs_file is the whole-file path; these pin the contract
    // that made the bug possible in the first place.

    #[tokio::test]
    async fn fs_file_returns_the_whole_file_where_fs_read_returns_a_head() {
        let (_d, srv, _home, _env) = scratch_server();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("long.tcl");
        let body: String = (0..300)
            .map(|i| format!("proc p{i} {{}} {{ return {i} }}\n"))
            .collect();
        std::fs::write(&path, &body).expect("write");

        let Json(v) = panes::fs_file(
            State(srv.clone()),
            Query(panes::FileQuery {
                path: path.to_string_lossy().into(),
            }),
        )
        .await;
        let text = v["text"].as_str().expect("text");
        assert_eq!(
            text.lines().count(),
            300,
            "the editor must see every line, not a head"
        );
        assert_eq!(text, body, "byte-for-byte, trailing newline included");
        assert_eq!(v["binary"], false);

        // The preview endpoint the editor used to share still serves a
        // head — that is its job; the point is that they are different
        // endpoints now.
        let Json(p) = panes::fs_read(
            State(srv),
            Query(panes::ReadFileQuery {
                path: path.to_string_lossy().into(),
                limit: 200,
            }),
        )
        .await;
        assert_eq!(p["text"].as_str().unwrap().lines().count(), 200);
    }

    #[tokio::test]
    async fn editor_save_refuses_a_stale_buffer_and_preserves_the_other_writer() {
        let (_d, srv, _home, _env) = scratch_server();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.tcl");
        std::fs::write(&path, "proc original {} {return 1}\n").unwrap();
        let Json(read) = panes::fs_file(
            State(srv.clone()),
            Query(panes::FileQuery {
                path: path.to_string_lossy().into(),
            }),
        )
        .await;
        let sha = read["sha256"].as_str().unwrap().to_string();
        std::fs::write(&path, "proc other_writer {} {return 2}\n").unwrap();
        let Json(conflict) = panes::fs_write(
            State(srv.clone()),
            Json(panes::WriteFileBody {
                path: path.to_string_lossy().into(),
                content: "proc stale {} {return 3}\n".into(),
                base_sha: Some(sha),
            }),
        )
        .await;
        assert_eq!(conflict["conflict"], true, "{conflict}");
        assert!(conflict["error"]
            .as_str()
            .unwrap()
            .contains("changed since"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "proc other_writer {} {return 2}\n"
        );
        let Json(fresh) = panes::fs_file(
            State(srv.clone()),
            Query(panes::FileQuery {
                path: path.to_string_lossy().into(),
            }),
        )
        .await;
        let Json(saved) = panes::fs_write(
            State(srv),
            Json(panes::WriteFileBody {
                path: path.to_string_lossy().into(),
                content: "proc reconciled {} {return 4}\n".into(),
                base_sha: Some(fresh["sha256"].as_str().unwrap().into()),
            }),
        )
        .await;
        assert_eq!(saved["ok"], true, "{saved}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "proc reconciled {} {return 4}\n"
        );
    }

    #[tokio::test]
    async fn fs_file_refuses_binary_and_missing() {
        let (_d, srv, _home, _env) = scratch_server();
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("blob.bin");
        std::fs::write(&bin, b"\x89PNG\x00\x00").expect("write");

        let Json(v) = panes::fs_file(
            State(srv.clone()),
            Query(panes::FileQuery {
                path: bin.to_string_lossy().into(),
            }),
        )
        .await;
        assert_eq!(v["binary"], true, "a NUL marks it binary, not editable");

        let Json(v) = panes::fs_file(
            State(srv),
            Query(panes::FileQuery {
                path: dir.path().join("nope.tcl").to_string_lossy().into(),
            }),
        )
        .await;
        assert!(v["error"].is_string(), "a missing file says so");
    }

    /// Tool tiering must not make a tool DISAPPEAR from the desktop's
    /// tool view — it moves a tool out of the prompt, not out of
    /// existence.
    ///
    /// /api/tools reported `registry.names()` (resident) plus the
    /// PROCESS-GLOBAL deferred registry (MCP), and tiering puts its
    /// deferred builtins in the SESSION registry's own map — a third
    /// set neither field covered. ~25 builtins were callable and listed
    /// nowhere, which is precisely the invisible-reduction this project
    /// exists to refuse.
    #[tokio::test]
    async fn api_tools_lists_tiered_builtins_somewhere() {
        let (_d, srv, _home, _env) = scratch_server();
        let Json(body) = list_tools(State(srv)).await;

        let names: Vec<String> = body["builtin"]
            .as_array()
            .unwrap()
            .iter()
            .chain(body["deferred"].as_array().unwrap().iter())
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();

        // Tools the core tier defers. Each must still be listed.
        for t in ["cronjob", "skill_manage", "web_search", "generate_image"] {
            assert!(
                names.iter().any(|n| n == t),
                "`{t}` vanished from /api/tools — resident: {:?}, deferred: {:?}",
                body["builtin"],
                body["deferred"]
            );
        }
        // And the everyday resident ones are still where they were.
        for t in ["terminal", "read_file", "tool_search"] {
            assert!(
                body["builtin"].as_array().unwrap().iter().any(|v| v == t),
                "`{t}` must stay resident"
            );
        }
        // The counts must describe the lists they ship with.
        assert_eq!(
            body["deferred_count"].as_u64().unwrap() as usize,
            body["deferred"].as_array().unwrap().len()
        );
        assert_eq!(
            body["builtin_count"].as_u64().unwrap() as usize,
            body["builtin"].as_array().unwrap().len()
        );
    }

    /// The pane's promote button is the command every other surface runs,
    /// and the route's whole job is to carry the flags — so that is what is
    /// pinned here. A body with `users: true` that quietly dropped the flag
    /// would look identical in the pane and write half of what the operator
    /// asked for, which is the failure mode worth a test.
    #[tokio::test]
    async fn acl_promote_route_carries_the_panes_flags_to_the_command() {
        let (_d, srv, _home, _env) = scratch_server();
        let path = veles_core::configedit::config_path();
        std::fs::write(
            &path,
            "# the operator's own note\n[acl]\nowner = [\"irc:me\"]\n",
        )
        .unwrap();
        veles_core::acl::invalidate_live_config();
        veles_core::pairing::grant(&srv.store, "bob!*@*", "trusted", None).unwrap();
        veles_core::pairing::user_save(
            &srv.store,
            "nyx",
            "trusted",
            &["nyx!~n@*.example.com".to_string()],
        )
        .unwrap();

        // The pane asks for the dry run first and shows it before offering
        // the write.
        let Json(dry) = acl_promote(
            State(srv.clone()),
            Json(AclPromoteBody {
                dry_run: true,
                include_expiring: false,
                users: true,
                prune: false,
            }),
        )
        .await;
        assert_eq!(dry["ok"], json!(true), "{dry}");
        assert_eq!(
            dry["wrote"],
            json!(false),
            "a dry run writes nothing: {dry}"
        );
        let text = dry["text"].as_str().unwrap();
        assert!(text.contains("dry run"), "{text}");
        assert!(
            text.contains("[[acl.entries]]") && text.contains("[[acl.users]]"),
            "both shapes the operator is about to get: {text}"
        );
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("[[acl"),
            "and the file is still the file: {}",
            std::fs::read_to_string(&path).unwrap()
        );

        // Then the write, with the users flag honoured.
        let Json(out) = acl_promote(
            State(srv.clone()),
            Json(AclPromoteBody {
                dry_run: false,
                include_expiring: false,
                users: true,
                prune: false,
            }),
        )
        .await;
        assert_eq!(out["wrote"], json!(true), "{out}");
        let text = out["text"].as_str().unwrap();
        assert!(
            text.contains("promoted 1 runtime grant and 1 runtime user into"),
            "{text}"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("identity = \"bob!*@*\"") && after.contains("name = \"nyx\""),
            "{after}"
        );
        assert!(
            after.contains("# the operator's own note"),
            "the surgical writer, not a re-serialization: {after}"
        );
        assert!(
            veles_core::pairing::list_grants(&srv.store).is_empty()
                && veles_core::pairing::list_users(&srv.store).is_empty(),
            "the store copies go, so the file is the one source"
        );
    }

    /// A refusal is a report, and the pane has to be able to tell it from a
    /// write: `ok` false, nothing written, and the reason in the text rather
    /// than in a code the pane would have to know.
    #[tokio::test]
    async fn acl_promote_route_reports_a_refusal_without_writing() {
        let (_d, srv, _home, _env) = scratch_server();
        // No `[acl]` at all: promoting would be the thing that turns
        // "everybody the adapter allows is owner" into "everybody the file
        // does not name is a guest".
        veles_core::acl::invalidate_live_config();
        veles_core::pairing::grant(&srv.store, "bob!*@*", "trusted", None).unwrap();
        let Json(out) = acl_promote(
            State(srv.clone()),
            Json(AclPromoteBody {
                dry_run: false,
                include_expiring: false,
                users: false,
                prune: false,
            }),
        )
        .await;
        assert_eq!(out["ok"], json!(false), "{out}");
        assert_eq!(out["wrote"], json!(false), "{out}");
        let text = out["text"].as_str().unwrap();
        assert!(text.contains("refusing"), "{text}");
        assert!(
            out["error"].as_str().unwrap().contains("refusing"),
            "the generic pane path reads `error`: {out}"
        );
        assert!(
            !veles_core::configedit::config_path().exists()
                || !std::fs::read_to_string(veles_core::configedit::config_path())
                    .unwrap()
                    .contains("[[acl.entries]]"),
            "nothing was written"
        );
    }

    /// `POST /api/triggers/new` must RETURN on a successful create.
    ///
    /// It deadlocked the whole gateway instead. The handler's success
    /// arm re-locked the store:
    ///
    /// ```text
    /// match srv.store.lock_ok().trigger_upsert(&row) {   // guard is a
    ///     Ok(_) => {                                      // match-scrutinee
    ///         let shown = srv.store.lock_ok()…            // temporary, alive
    /// ```                                                 // for the WHOLE match
    ///
    /// The `MutexGuard` in the scrutinee is a temporary Rust keeps alive
    /// until the end of the entire `match` (the 2021 temporary-scope
    /// rule). The `Ok` arm then locks the SAME non-reentrant
    /// `std::sync::Mutex` on the SAME thread, so `trigger_upsert` had
    /// already committed the row but the handler blocked forever holding
    /// the lock — and because every surface shares that one store
    /// mutex, the gateway froze on the first live `POST`. Observed
    /// 2026-09-14: a lone `0` in the log, then nothing, SIGKILL.
    ///
    /// The fix is one lock for the whole handler. The test pins the
    /// RETURN, not the internals: run the handler on its own OS thread
    /// with a deadline, so a deadlock fails the assertion rather than
    /// hanging the suite. `tokio::time::timeout` around the await would
    /// not work — a deadlock parks the thread mid-poll and never yields
    /// to the timer.
    #[test]
    fn trigger_new_returns_on_success_instead_of_relocking_the_store() {
        let (_d, srv, _home, _env) = scratch_server();
        // Keep a handle on the SAME store Arc the handler locks. The
        // regression is that the handler never RELEASES it, so the
        // proof is that a fresh lock of THIS store succeeds afterwards
        // — a throwaway store would prove nothing.
        let store = srv.store.clone();
        let body = json!({
            "name": "deadlock-probe",
            "platform": "irc",
            "network": "irc.synirc.net",
            "channel": "#flashsupport",
            "event": "TEXT",
            "match_kind": "always",
            "action": "slopdrop",
        });

        let (tx, rx) = std::sync::mpsc::channel::<(StatusCode, Value)>();
        // The handler runs on a dedicated OS thread with its own
        // runtime. If it deadlocks on the store mutex, THIS thread
        // hangs — but `rx.recv_timeout` below still returns and the
        // test fails loudly instead of blocking the suite.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let (code, Json(v)) = rt.block_on(trigger_new(State(srv), Json(body)));
            let _ = tx.send((code, v));
        });

        let (code, v) = rx.recv_timeout(std::time::Duration::from_secs(5)).expect(
            "POST /api/triggers/new deadlocked: the success arm re-locked the \
                 store mutex the match scrutinee still holds",
        );
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(
            v["name"], "deadlock-probe",
            "the created row reads back: {v}"
        );

        // The store is still usable afterwards — the lock was RELEASED,
        // not merely not-poisoned. The deadlocked handler held this
        // mutex forever; `try_lock` is the assertion that it let go.
        assert!(
            store.try_lock().is_ok(),
            "store mutex is still held after trigger_new returned — it was never released"
        );
    }

    /// A CHILD SESSION RUNS IN ANOTHER PROCESS, so its events are
    /// published to a bus this one does not subscribe to.
    ///
    /// `session_tap` is process-wide by construction ("A process-wide
    /// `broadcast` channel"), and a delegate or /btw child is a
    /// separate `veles chat -q`. There is no events table in the store
    /// and no relay, so an attached child replayed once at attach and
    /// then sat still for the whole turn — USER 2026-09-18: "I could
    /// see its progress but then at some point it stopped streaming
    /// and looked hung even though there were new messages coming in".
    ///
    /// The stream therefore FOLLOWS THE STORE when the bus is not the
    /// source: rows that appear with no bus traffic behind them go out
    /// as ordinary `history` frames, closed by `history-appended`.
    #[tokio::test]
    async fn a_session_written_by_another_process_still_reaches_an_attached_stream() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        std::env::set_var("VELES_SESSION_FOLLOW_MS", "50");
        let sid = "followed-child";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("go"))
            .expect("the question");
        let base = srv.store.lock_ok().last_msg_id(sid).expect("head");

        let sse = session_events(
            State(srv.clone()),
            Path(sid.to_string()),
            Query(SessionEventsQuery {
                since: base,
                since_note: 0,
            }),
        )
        .await;
        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut appended: Vec<Value> = Vec::new();
        let mut cursor: i64 = 0;
        let mut live = false;
        let mut wrote = false;
        loop {
            // The write happens AFTER the live boundary, exactly as a
            // child process would do it: straight into the store, with
            // nothing published on this process's bus.
            if live && !wrote {
                wrote = true;
                srv.store
                    .lock_ok()
                    .append_message(
                        sid,
                        &veles_core::providers::ChatMessage::assistant("the child spoke", None),
                    )
                    .expect("the child's row");
            }
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), body.next())
                .await
                .expect("frames within 10s")
                .expect("body still open")
                .expect("bytes");
            raw.push_str(&String::from_utf8_lossy(&chunk));
            let mut done = false;
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                let (mut kind, mut data) = ("", "");
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = v.trim();
                    }
                }
                match kind {
                    "session-live" => live = true,
                    "history" if live => {
                        appended.push(serde_json::from_str(data).expect("row json"))
                    }
                    "history-appended" => {
                        let v: Value = serde_json::from_str(data).expect("appended json");
                        cursor = v["last_msg_id"].as_i64().unwrap_or(0);
                        done = true;
                    }
                    _ => {}
                }
            }
            if done {
                break;
            }
        }
        std::env::remove_var("VELES_SESSION_FOLLOW_MS");
        assert_eq!(appended.len(), 1, "exactly the gap, once: {appended:?}");
        assert!(
            appended[0].to_string().contains("the child spoke"),
            "the row the other process wrote: {appended:?}"
        );
        assert!(
            cursor > base,
            "the boundary must carry the new head so the client's cursor \
             moves with it: {cursor} vs {base}"
        );
    }

    /// …and the other direction, which is the one that protects the
    /// transcript: a turn running in THIS process already reaches the
    /// client as live events and is reconciled by its own turn-result.
    /// Sending its rows again as history would paint the turn twice.
    ///
    /// THIS HALF pins the BUS check on its own — there is deliberately
    /// no live snapshot here, so if `bus_quiet` stopped being consulted
    /// the row below would go out as history. (The companion test pins
    /// the snapshot check the same way, with traffic removed instead.)
    #[tokio::test]
    async fn a_turn_whose_rows_the_bus_already_carried_is_not_replayed() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        std::env::set_var("VELES_SESSION_FOLLOW_MS", "50");
        let sid = "local-turn";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("go"))
            .expect("the question");
        let base = srv.store.lock_ok().last_msg_id(sid).expect("head");

        let sse = session_events(
            State(srv.clone()),
            Path(sid.to_string()),
            Query(SessionEventsQuery {
                since: base,
                since_note: 0,
            }),
        )
        .await;
        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut appended = 0usize;
        let mut deltas = 0usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
        // A local turn: a row persists mid-turn (the round the loop just
        // finished) and the bus carries its text.
        srv.store
            .lock_ok()
            .append_message(
                sid,
                &veles_core::providers::ChatMessage::assistant("mid-turn row", None),
            )
            .expect("the round's row");
        veles_core::session_tap::publish_stream(
            sid,
            &veles_core::providers::StreamEvent::Delta("tok".into()),
        );
        while std::time::Instant::now() < deadline {
            let Ok(Some(Ok(chunk))) =
                tokio::time::timeout(std::time::Duration::from_millis(300), body.next()).await
            else {
                continue;
            };
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        match v.trim() {
                            "history-appended" => appended += 1,
                            "delta" => deltas += 1,
                            _ => {}
                        }
                    }
                }
            }
        }
        std::env::remove_var("VELES_SESSION_FOLLOW_MS");
        assert!(deltas > 0, "the bus half of the test actually ran");
        assert_eq!(
            appended, 0,
            "a turn in this process owns its own transcript; the follow \
             tick must stay silent or every round is painted twice"
        );
    }

    /// …and the SNAPSHOT check on its own: a turn in this process that
    /// is simply quiet.
    ///
    /// A long tool call produces no bus events for many ticks while its
    /// result row lands in the store. "The bus said nothing" is
    /// therefore not enough to conclude "another process wrote this" —
    /// without the live-turn check, a local turn's own round would come
    /// back at it as a `history` frame and be painted a second time.
    /// There is deliberately no bus traffic in this test.
    #[tokio::test]
    async fn a_silent_local_turn_is_not_replayed_as_history_either() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        std::env::set_var("VELES_SESSION_FOLLOW_MS", "50");
        let sid = "silent-local-turn";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("go"))
            .expect("the question");
        let base = srv.store.lock_ok().last_msg_id(sid).expect("head");
        veles_core::session_tap::turn_started(sid, "desktop", base, "");

        let sse = session_events(
            State(srv.clone()),
            Path(sid.to_string()),
            Query(SessionEventsQuery {
                since: base,
                since_note: 0,
            }),
        )
        .await;
        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut appended = 0usize;
        // The round the loop just finished, persisted mid-turn. No
        // delta, no tool event — the provider has said nothing for a
        // while, which is exactly what a long tool call looks like.
        srv.store
            .lock_ok()
            .append_message(
                sid,
                &veles_core::providers::ChatMessage::assistant("mid-turn row", None),
            )
            .expect("the round's row");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
        while std::time::Instant::now() < deadline {
            let Ok(Some(Ok(chunk))) =
                tokio::time::timeout(std::time::Duration::from_millis(300), body.next()).await
            else {
                continue;
            };
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        if v.trim() == "history-appended" {
                            appended += 1;
                        }
                    }
                }
            }
        }
        veles_core::session_tap::turn_ended(sid);
        std::env::remove_var("VELES_SESSION_FOLLOW_MS");
        assert_eq!(
            appended, 0,
            "a live turn in this process owns its transcript even while \
             it is quiet; the tick must resync and say nothing"
        );
    }

    /// …but only while it is actually alive.
    ///
    /// The snapshot is the tick's evidence that a local turn owns the
    /// transcript, and `turn_ended` is the only thing that clears it.
    /// A turn that dies without getting there — a panic, a killed
    /// child, a gateway that lost the writer — leaves the evidence
    /// standing forever, and the tick then never sends a gap again for
    /// that session. That is the "attached and it just stopped
    /// streaming" report, re-armed by the fix for it.
    ///
    /// A stale snapshot must therefore stop counting, and the rows the
    /// dead turn left behind must still reach the client.
    #[tokio::test]
    async fn a_turn_whose_snapshot_went_stale_stops_owning_the_transcript() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        std::env::set_var("VELES_SESSION_FOLLOW_MS", "50");
        // Any snapshot is stale the moment it is looked at.
        std::env::set_var("VELES_SNAPSHOT_STALE_MS", "1");
        let sid = "stale-turn-owns-nothing";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("go"))
            .expect("the question");
        let base = srv.store.lock_ok().last_msg_id(sid).expect("head");
        // A turn that started and will never end: exactly the corpse.
        veles_core::session_tap::turn_started(sid, "desktop", base, "");

        let sse = session_events(
            State(srv.clone()),
            Path(sid.to_string()),
            Query(SessionEventsQuery {
                since: base,
                since_note: 0,
            }),
        )
        .await;
        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let mut appended = 0usize;
        srv.store
            .lock_ok()
            .append_message(
                sid,
                &veles_core::providers::ChatMessage::assistant("row the dead turn left", None),
            )
            .expect("the orphaned row");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
        while std::time::Instant::now() < deadline && appended == 0 {
            let Ok(Some(Ok(chunk))) =
                tokio::time::timeout(std::time::Duration::from_millis(300), body.next()).await
            else {
                continue;
            };
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        if v.trim() == "history-appended" {
                            appended += 1;
                        }
                    }
                }
            }
        }
        veles_core::session_tap::turn_ended(sid);
        std::env::remove_var("VELES_SESSION_FOLLOW_MS");
        std::env::remove_var("VELES_SNAPSHOT_STALE_MS");
        assert!(
            appended >= 1,
            "a snapshot nobody has touched is not a live turn — the row \
             it left behind has to reach the client, or the window waits \
             on a corpse forever"
        );
    }

    /// A module's OPEN route (`/api/plugins/<name>/open/<route>`) is the
    /// one module surface outside the bearer gate: the module checks a
    /// credential of its own (the slopdrop bridge's per-evaluation
    /// Bluesky read grant), which arrives as the request's bearer. Its
    /// ordinary routes stay gated, a near-miss path stays gated, and a
    /// module that serves no open route answers 404.
    #[tokio::test]
    async fn a_modules_open_route_skips_the_bearer_gate_and_nothing_else_does() {
        use tower::ServiceExt as _;
        struct Opened;
        impl veles_core::moduleroutes::ModuleRoutes for Opened {
            fn handle<'a>(
                &'a self,
                _req: veles_core::moduleroutes::RouteRequest,
            ) -> veles_core::moduleroutes::RouteFuture<'a> {
                Box::pin(async {
                    veles_core::moduleroutes::RouteResponse::ok(json!({"gated": true}))
                })
            }
            fn handle_open<'a>(
                &'a self,
                req: veles_core::moduleroutes::RouteRequest,
            ) -> veles_core::moduleroutes::RouteFuture<'a> {
                Box::pin(async move {
                    veles_core::moduleroutes::RouteResponse::ok(
                        json!({"path": req.path, "bearer": req.bearer, "body": req.body}),
                    )
                })
            }
        }
        struct Closed;
        impl veles_core::moduleroutes::ModuleRoutes for Closed {
            fn handle<'a>(
                &'a self,
                _req: veles_core::moduleroutes::RouteRequest,
            ) -> veles_core::moduleroutes::RouteFuture<'a> {
                Box::pin(async { veles_core::moduleroutes::RouteResponse::ok(json!({})) })
            }
        }
        veles_core::moduleroutes::register_routes("opened-test", Arc::new(Opened));
        veles_core::moduleroutes::register_routes("closed-test", Arc::new(Closed));
        let (_d, srv, _home, _env) = scratch_server();
        let auth = Arc::new(veles_core::authn::AuthConfig::new(
            Default::default(),
            "secret-token".to_string(),
        ));
        let app = router(srv.clone(), auth);
        let req = |method: &str, path: &str, bearer: Option<&str>| {
            let mut b = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json");
            if let Some(t) = bearer {
                b = b.header("authorization", format!("Bearer {t}"));
            }
            b.body(axum::body::Body::from(r#"{"method":"m"}"#)).unwrap()
        };
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/plugins/opened-test/open/read",
                Some("grant-1"),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "no owner bearer needed");
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap())
                .unwrap();
        assert_eq!(body["path"], "/read");
        assert_eq!(
            body["bearer"], "grant-1",
            "the module gets its own credential"
        );
        assert_eq!(body["body"]["method"], "m");
        for (path, why) in [
            (
                "/api/plugins/opened-test/status",
                "an ordinary module route",
            ),
            ("/api/plugins/opened-test/open/read/more", "a deeper path"),
            ("/api/plugins/opened-test/open", "no route after open"),
        ] {
            let r = app
                .clone()
                .oneshot(req("GET", path, Some("grant-1")))
                .await
                .unwrap();
            assert_eq!(r.status(), 401, "{why} stays behind the gate: {path}");
        }
        let r = app
            .clone()
            .oneshot(req("POST", "/api/plugins/closed-test/open/read", None))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            404,
            "a module that serves no open route serves nothing"
        );
        for path in ["/api/install/download", "/api/install/download/confirm"] {
            let r = app
                .clone()
                .oneshot(req("POST", path, Some("grant-1")))
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                401,
                "signed bootstrap is not an open module route: {path}"
            );
            let r = app
                .clone()
                .oneshot(req("POST", path, Some("secret-token")))
                .await
                .unwrap();
            assert_ne!(
                r.status(),
                401,
                "the owner bearer may reach the installer: {path}"
            );
        }
    }

    /// The challenge relay page (Part E) needs no bearer token — it is
    /// opened from a chat client — but only its random token opens it,
    /// and it answers the very ask the surfaces were asked: a click on
    /// the picture is `click X Y`, the verdict buttons are choices, and
    /// anything else is refused rather than passed along as an answer.
    #[tokio::test]
    async fn the_challenge_page_answers_its_ask_and_nothing_else() {
        use tower::ServiceExt as _;
        let (_d, srv, _home, _env) = scratch_server();
        // A token-protected API: the page must still open without it.
        let auth = Arc::new(veles_core::authn::AuthConfig::new(
            Default::default(),
            "secret-token".to_string(),
        ));
        let app = router(srv.clone(), auth);
        let req = veles_core::ask::AskRequest::new(
            "page-sess",
            veles_core::ask::AskKind::Challenge,
            "cloudflare challenge on shop.example — solve it for the agent",
            vec![],
        );
        let id = req.id.clone();
        let leg = veles_core::ask::register(&req, None, None).expect("registered");
        veles_core::challenge::put_page_for_tests(
            "tok123",
            veles_core::challenge::Page {
                ask_id: id,
                image: "iVBORw0KGgo=".into(),
                prompt: req.prompt.clone(),
                round: 1,
                window_offered: false,
            },
        );
        let get = |path: &str| {
            axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let post = |path: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        };
        let r = app.clone().oneshot(get("/challenge/tok123")).await.unwrap();
        assert_eq!(r.status(), 200, "the page opens with no bearer token");
        let r = app
            .clone()
            .oneshot(get("/challenge/nottoken"))
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "a wrong token opens nothing");
        let r = app.clone().oneshot(get("/api/status")).await.unwrap();
        assert_eq!(
            r.status(),
            401,
            "and the exemption is the page, not the API"
        );
        let r = app
            .clone()
            .oneshot(get("/challenge/tok123/shot"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let r = app
            .clone()
            .oneshot(post(
                "/challenge/tok123/answer",
                json!({ "text": "rm -rf /" }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "not an action on the page, not an answer");
        let r = app
            .clone()
            .oneshot(post(
                "/challenge/tok123/answer",
                json!({ "text": "click 200 140" }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let answer = leg.wait().await;
        assert_eq!(answer.free_text.as_deref(), Some("click 200 140"));
    }

    /// The page's words come from the engine catalogs in the gateway's
    /// language, and its wire protocol does not translate (Step 5.4).
    /// Before this, the served HTML was `include_str!`'d English: a
    /// German gateway handed a phone an English page while its own
    /// desktop dialog said "Dies für den Agenten lösen".
    ///
    /// Scoped, not switched: the locale is process-global and cargo
    /// runs tests in parallel threads, so `set_active` here would leak
    /// into every neighbour. `ScopeGuard` is the thread-local half the
    /// desktop's requests use.
    ///
    /// The page is fetched from the ROUTER, not read out of the helper:
    /// the claim is about what a phone is served, and a regression that
    /// serves the `include_str!`'d English page straight from the
    /// handler must go red here.
    #[tokio::test]
    async fn the_challenge_page_speaks_the_gateways_language_and_keeps_the_protocol_english() {
        use tower::ServiceExt as _;
        let (_d, srv, _home, _env) = scratch_server();
        let app = router(
            srv.clone(),
            Arc::new(veles_core::authn::AuthConfig::new(
                Default::default(),
                "secret-token".to_string(),
            )),
        );
        veles_core::challenge::put_page_for_tests(
            "langtok",
            veles_core::challenge::Page {
                ask_id: "lang-ask".into(),
                image: "iVBORw0KGgo=".into(),
                prompt: "solve it".into(),
                round: 1,
                window_offered: false,
            },
        );
        let get_page = || {
            axum::http::Request::builder()
                .uri("/challenge/langtok")
                .body(axum::body::Body::empty())
                .unwrap()
        };

        // The words a German gateway serves are German, from the engine
        // catalogs — the same ones its desktop dialog uses.
        let _de = veles_core::i18n::ScopeGuard::enter(Some("de".into()));
        let res = app.clone().oneshot(get_page()).await.unwrap();
        assert_eq!(res.status(), 200);
        let page = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        for german in [
            "Dies für den Agenten lösen",
            "Eingeben",
            "Gelöst",
            "Aufgeben",
            "Fenster öffnen",
            "einzugebender Text",
            "Diese Challenge ist vorbei.",
        ] {
            assert!(
                page.contains(german),
                "the page speaks German: {german} missing"
            );
        }
        // Not English, so this is translation and not a lucky substring.
        for english in ["Solved", "Give up", "Open a window", "text to type"] {
            assert!(
                !page.contains(english),
                "an English word survived the catalog: {english}"
            );
        }
        // The protocol the server parses stays English, in the JS.
        for protocol in [
            "'click ' + x + ' ' + y",
            "'type ' + t",
            "'key ' + b.dataset.key",
            "{ choice: 'solved' }",
            "{ choice: 'give_up' }",
            "{ choice: 'window' }",
        ] {
            assert!(
                page.contains(protocol),
                "the wire protocol must not translate: {protocol}"
            );
        }
        // Every placeholder was substituted.
        assert!(!page.contains("{L:"), "a placeholder survived");
        // And the JS dict it became is parseable JSON with the German
        // words in it — a translation carrying a quote or a newline
        // must not break the page.
        let start = page.find("const L = ").expect("the dict");
        let json = &page[start + "const L = ".len()..];
        let end = json.find(";\n").expect("the dict ends");
        let dict: Value = serde_json::from_str(&json[..end]).expect("the dict parses");
        assert_eq!(dict["solved"].as_str(), Some("Gelöst"));

        // A locale with no catalog at all still serves a whole page, in
        // English: the loud-missing fallback returns the key, and these
        // keys ARE the English text, so no placeholder can survive.
        drop(_de);
        let _none = veles_core::i18n::ScopeGuard::enter(Some("xx".into()));
        let res = app.clone().oneshot(get_page()).await.unwrap();
        let page = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            page.contains(">Solved<") && !page.contains("{L:"),
            "a locale with no catalog still serves a whole page"
        );
    }

    /// A note the bus already delivered is not sent again when the
    /// stream goes quiet; a note that never rode this bus still is.
    ///
    /// USER 2026-09-30: "the guards are still dumped again at the end of
    /// the agent turn all at once". The note cursor is held behind the
    /// notes a turn emits so that another process's notes are not jumped
    /// over, and the first quiet tick after the turn therefore re-sent
    /// every note the turn had already shown live. The desktop painted
    /// them all a second time, in one heap under the answer — reproduced
    /// on the real desktop against a real gateway before this test.
    #[tokio::test]
    async fn a_note_the_bus_delivered_is_not_sent_again_when_the_stream_goes_quiet() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        std::env::set_var("VELES_SESSION_FOLLOW_MS", "50");
        let sid = "bus-note-once";
        srv.store.lock_ok().create_session(sid).expect("create");
        srv.store
            .lock_ok()
            .append_message(sid, &veles_core::providers::ChatMessage::user("go"))
            .expect("the question");
        let base = srv.store.lock_ok().last_msg_id(sid).expect("head");
        let sse = session_events(
            State(srv.clone()),
            Path(sid.to_string()),
            Query(SessionEventsQuery {
                since: base,
                since_note: 0,
            }),
        )
        .await;
        // A note the turn emitted: persisted, then published with its id,
        // exactly as Agent::note does.
        let live = srv
            .store
            .lock_ok()
            .record_note(sid, "guard", "shown live", None, None)
            .expect("live note");
        let mut n =
            veles_core::notes::EngineNote::new(veles_core::notes::NoteKind::Guard, "shown live");
        n.id = live;
        veles_core::session_tap::publish_stream(sid, &veles_core::providers::StreamEvent::Note(n));
        // A note another process wrote: in the store, never on this bus.
        let other = srv
            .store
            .lock_ok()
            .record_note(sid, "guard", "written elsewhere", None, None)
            .expect("other note");

        let mut body = sse.into_response().into_body().into_data_stream();
        let mut raw = String::new();
        let (mut live_events, mut live_again, mut other_rows, mut appended) = (0, 0, 0, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < deadline && appended == 0 {
            let Ok(Some(Ok(chunk))) =
                tokio::time::timeout(std::time::Duration::from_millis(300), body.next()).await
            else {
                continue;
            };
            raw.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(cut) = raw.find("\n\n") {
                let record: String = raw.drain(..cut + 2).collect();
                let mut kind = "";
                let mut data = Value::Null;
                for line in record.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = serde_json::from_str(v.trim()).unwrap_or(Value::Null);
                    }
                }
                match kind {
                    "note" if data["id"] == json!(live) => live_events += 1,
                    "history" if data["note_id"] == json!(live) => live_again += 1,
                    "history" if data["note_id"] == json!(other) => other_rows += 1,
                    "history-appended" => appended += 1,
                    _ => {}
                }
            }
        }
        std::env::remove_var("VELES_SESSION_FOLLOW_MS");
        assert_eq!(
            live_events, 1,
            "the live note reaches the client once, live"
        );
        assert_eq!(appended, 1, "the quiet tick ran");
        assert_eq!(
            live_again, 0,
            "a note this stream already delivered is not sent again — the \
             desktop paints every copy it is sent"
        );
        assert_eq!(
            other_rows, 1,
            "a note that never rode this bus still arrives: that is what \
             the note cursor is held back for"
        );
    }

    // ── the child → gateway relay (2026-09-19) ──────────────────────

    /// The point of the whole relay: an event that happened in ANOTHER
    /// process reaches a window attached here. Without it a child's
    /// deltas go to a bus in the child's own process with no
    /// subscribers, and the attached window shows the store replay and
    /// then nothing (USER 2026-09-18, "it stopped streaming").
    #[tokio::test]
    async fn an_event_relayed_from_a_child_process_reaches_an_attached_stream() {
        use futures::StreamExt;
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "relayed-into-the-gateway";
        srv.store.lock_ok().create_session(sid).expect("create");
        let base = srv.store.lock_ok().last_msg_id(sid).unwrap_or(0);

        let sse = session_events(
            State(srv.clone()),
            Path(sid.to_string()),
            Query(SessionEventsQuery {
                since: base,
                since_note: 0,
            }),
        )
        .await;
        let mut body = sse.into_response().into_body().into_data_stream();

        // What a child's forwarder POSTs: its own bus events, verbatim.
        let batch = vec![veles_core::session_tap::SessionEvent {
            seq: 1,
            session_id: sid.to_string(),
            kind: "delta".to_string(),
            data: json!({ "text": "from the child" }),
        }];
        let code = relay_ingest(State(srv.clone()), Json(batch)).await;
        assert_eq!(code, StatusCode::NO_CONTENT);

        let mut raw = String::new();
        let mut saw = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < deadline && !saw {
            let Ok(Some(Ok(chunk))) =
                tokio::time::timeout(std::time::Duration::from_millis(300), body.next()).await
            else {
                continue;
            };
            raw.push_str(&String::from_utf8_lossy(&chunk));
            if raw.contains("from the child") {
                saw = true;
            }
        }
        assert!(
            saw,
            "a relayed delta must reach the attached stream: {raw:?}"
        );
    }

    /// `seq` is monotonic WITHIN A PROCESS RUN. The child's counter
    /// starts at 1 in its own process and means nothing here, so
    /// echoing it would hand subscribers a sequence that jumps
    /// backwards the moment a child starts. Republishing through
    /// `session_tap::publish` restamps it with this bus's own.
    #[tokio::test]
    async fn a_relayed_event_is_restamped_with_this_buss_sequence() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "relay-restamp";
        let mut rx = veles_core::session_tap::subscribe();
        // Move this bus's counter well past anything the child sends.
        for _ in 0..5 {
            veles_core::session_tap::publish(sid, "delta", json!({ "text": "local" }));
        }
        let before = veles_core::session_tap::latest_seq();
        let batch = vec![veles_core::session_tap::SessionEvent {
            seq: 1,
            session_id: sid.to_string(),
            kind: "delta".to_string(),
            data: json!({ "text": "child" }),
        }];
        relay_ingest(State(srv.clone()), Json(batch)).await;
        let mut got = None;
        while let Ok(ev) = rx.try_recv() {
            if ev.session_id == sid && ev.data["text"] == "child" {
                got = Some(ev);
            }
        }
        let ev = got.expect("the relayed event was republished");
        assert!(
            ev.seq > before,
            "the child's seq 1 must not be echoed: got {} against a bus at {before}",
            ev.seq
        );
    }

    /// A child's `turn-started` has to become a real snapshot here, or
    /// a window attaching mid-turn is handed nothing and the
    /// staleness guard has no turn to keep fresh either.
    #[tokio::test]
    async fn a_relayed_turn_start_registers_a_live_turn_here() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "relay-turn-lifecycle";
        assert!(
            veles_core::session_tap::snapshot(sid).is_none(),
            "clean start"
        );
        let started = vec![veles_core::session_tap::SessionEvent {
            seq: 1,
            session_id: sid.to_string(),
            kind: "turn-started".to_string(),
            data: json!({ "surface": "subagent", "base_msg_id": 7,
                          "prompt_msg_id": 9, "prompt": "go" }),
        }];
        let mut rx = veles_core::session_tap::subscribe();
        relay_ingest(State(srv.clone()), Json(started)).await;
        let snap = veles_core::session_tap::snapshot(sid).expect("a relayed turn is a live turn");
        assert_eq!(snap.surface, "subagent", "the child's surface carries");
        assert_eq!(snap.base_msg_id, 7);
        let relayed = loop {
            let ev = rx.recv().await.expect("republished turn-started");
            if ev.session_id == sid && ev.kind == "turn-started" {
                break ev;
            }
        };
        assert_eq!(relayed.data["prompt_msg_id"], json!(9));
        assert_eq!(relayed.data["base_msg_id"], json!(7));

        let ended = vec![veles_core::session_tap::SessionEvent {
            seq: 2,
            session_id: sid.to_string(),
            kind: "turn-ended".to_string(),
            data: json!({}),
        }];
        relay_ingest(State(srv.clone()), Json(ended)).await;
        assert!(
            veles_core::session_tap::snapshot(sid).is_none(),
            "and turn-ended clears it, or the staleness guard is the only \
             thing standing between a child and a wedged window"
        );
    }

    /// The child names its pid and host in `turn-started`, and the
    /// mirror keeps them: the owner is what lets a child killed at its
    /// timeout (no `turn-ended`, ever) stop showing as running here.
    #[tokio::test]
    async fn a_relayed_turn_is_mirrored_with_its_owner() {
        let (_d, srv, _home, _env) = scratch_server();
        let sid = "relay-turn-owner";
        let host = veles_core::platform::hostname();
        let started = vec![veles_core::session_tap::SessionEvent {
            seq: 1,
            session_id: sid.to_string(),
            kind: "turn-started".to_string(),
            data: json!({ "surface": "subagent", "base_msg_id": 0, "prompt": "",
                          "pid": std::process::id(), "host": host }),
        }];
        relay_ingest(State(srv.clone()), Json(started)).await;
        let snap = veles_core::session_tap::snapshot(sid).expect("a relayed turn is a live turn");
        assert_eq!(
            snap.owner_pid,
            Some(std::process::id()),
            "the mirror watches the child that runs the turn"
        );
        veles_core::session_tap::turn_ended(sid);
    }

    /// The desktop's default writer IS `/defaults`: one write of both
    /// `[model]` keys, through the engine's resolver.
    #[tokio::test]
    async fn the_defaults_endpoint_writes_through_the_defaults_command() {
        let (_d, srv, _home, _env) = scratch_server();
        let path = veles_core::configedit::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[model]\nprovider = \"a\"\ndefault = \"m1\"\n\n\
             [[custom_providers]]\nname = \"a\"\nbase_url = \"http://127.0.0.1:0/v1\"\nmodels = [\"m1\"]\n\n\
             [[custom_providers]]\nname = \"b\"\nbase_url = \"http://127.0.0.1:2/v1\"\nmodels = [\"m2\"]\n",
        )
        .unwrap();
        let Json(v) =
            defaults_run(State(srv.clone()), Json(json!({ "command": "model m2@b" }))).await;
        let text = v["text"].as_str().unwrap_or_default().to_string();
        assert!(text.contains("model.default = m2"), "{text}");
        let cfg: VelesConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.model.default.as_deref(), Some("m2"));
        assert!(
            cfg.model
                .provider
                .as_deref()
                .is_some_and(|p| p.ends_with('b')),
            "{text}"
        );
    }

    /// The relay endpoint is a write path reachable from any process
    /// on the box, so it lives behind the same bearer as everything
    /// else. Pinned rather than assumed: it is on the blanket auth
    /// layer today, and a future route added outside that layer would
    /// be silently open.
    #[tokio::test]
    async fn the_relay_endpoint_is_behind_the_bearer() {
        let token = "relay-token";
        let (_d, srv, _home, _env) = scratch_server();
        let auth = std::sync::Arc::new(veles_core::authn::AuthConfig::new(
            Default::default(),
            token.to_string(),
        ));
        let app = router(srv, auth);
        let body = serde_json::to_string(&vec![veles_core::session_tap::SessionEvent {
            seq: 1,
            session_id: "s".into(),
            kind: "delta".into(),
            data: json!({}),
        }])
        .unwrap();
        let unauthed = axum::http::Request::builder()
            .method("POST")
            .uri(veles_core::relay::RELAY_PATH)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.clone()))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app.clone(), unauthed)
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "anyone on the box could otherwise inject events into any session"
        );

        let authed = axum::http::Request::builder()
            .method("POST")
            .uri(veles_core::relay::RELAY_PATH)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, authed).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    // ── the plan split on /api/usage (2026-09-20) ────────────────────

    /// Seed a store with calls spread over the four plans (and one
    /// provider no plan claims), then write a session row so the
    /// session-scoped half has something to group.
    fn seed_plan_ledger(store: &Mutex<Store>) {
        let tok = |p: u64, c: u64| veles_core::store::CallTokens {
            prompt_tokens: p,
            completion_tokens: c,
            cached_tokens: 0,
            cache_write_tokens: 0,
            cache_reported: false,
            reasoning_tokens: None,
        };
        let s = store.lock().unwrap();
        s.create_session("split-session").expect("session");
        for (prov, model, calls) in [
            ("ali-token-plan-personal", "qwen3.8-max", 4),
            ("ali-token-plan-team", "deepseek-v4.1-flash", 3),
            ("ali-responses", "qwen3.8-max", 1),
            ("zai-coding-plan", "glm-5.2", 2),
            ("kimi-coding-plan", "k3", 1),
            ("openrouter", "some-model", 2),
        ] {
            for _ in 0..calls {
                s.record_usage("split-session", prov, model, tok(100, 10))
                    .expect("usage");
            }
        }
    }

    /// THE COMPLAINT, over the wire: the desktop's Usage widget reads
    /// `/api/usage`, and a single total across four disjoint bills is
    /// what the user called confusing. The split is served as named
    /// rows, each carrying the providers that fed it.
    #[tokio::test]
    async fn usage_serves_the_ledger_split_by_plan() {
        let (_d, srv, _home, _env) = scratch_server();
        seed_plan_ledger(&srv.store);
        let token = "usage-split-token";
        let app = router(srv.clone(), std::sync::Arc::new(token_cfg(token)));

        let res = tower::ServiceExt::oneshot(app, probe_request("/api/usage", Some(token), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).expect("json");

        let rows = v["by_plan"].as_array().expect("by_plan is a list");
        assert_eq!(
            rows.len(),
            5,
            "four plans plus the unclaimed provider: {v:#?}"
        );
        let find = |name: &str| {
            rows.iter()
                .find(|r| r["plan"] == name)
                .unwrap_or_else(|| panic!("no row for {name}: {v:#?}"))
        };
        // `ali-responses` is a TEAM credential, so its calls land in the
        // team row — the exact conflation the split exists to expose.
        let team = find("team");
        assert_eq!(team["calls"], 4, "3 team + 1 ali-responses");
        assert_eq!(team["providers"][0], "ali-responses");
        assert_eq!(team["providers"][1], "ali-token-plan-team");
        let personal = find("personal");
        assert_eq!(personal["calls"], 4, "and it is NOT the same pool");
        assert_eq!(find("zai")["calls"], 2);
        assert_eq!(find("kimi")["calls"], 1);

        // The unclaimed provider keeps its own identity rather than
        // being folded into a plan it does not belong to.
        let unclaimed = rows
            .iter()
            .find(|r| r["plan"].is_null())
            .expect("the openrouter row");
        assert_eq!(unclaimed["providers"][0], "openrouter");
        assert_eq!(unclaimed["calls"], 2);

        // The flat totals are still there — the status bar abbreviates
        // them and a caller may want both views.
        assert_eq!(v["calls"], 13);
        assert!(v["prompt_tokens"].as_u64().unwrap() > 0);
    }

    /// The session-scoped half is a real split too, not a copy of the
    /// global one: the widget shows "by plan — this session" and it has
    /// to answer about the conversation on screen.
    #[tokio::test]
    async fn usage_scopes_the_split_to_the_session_asked_about() {
        let (_d, srv, _home, _env) = scratch_server();
        seed_plan_ledger(&srv.store);
        let token = "usage-split-token";
        {
            // A second session on a different plan, so a wrong scope is
            // visible rather than coincidentally equal.
            let s = srv.store.lock().unwrap();
            s.create_session("other-session").expect("session");
            for _ in 0..5 {
                s.record_usage(
                    "other-session",
                    "kimi-coding-plan",
                    "k3",
                    veles_core::store::CallTokens {
                        prompt_tokens: 50,
                        completion_tokens: 5,
                        cached_tokens: 0,
                        cache_write_tokens: 0,
                        cache_reported: false,
                        reasoning_tokens: None,
                    },
                )
                .expect("usage");
            }
        }

        let res = tower::ServiceExt::oneshot(
            router(srv.clone(), std::sync::Arc::new(token_cfg(token))),
            probe_request("/api/usage?session=split-session", Some(token), None),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).expect("json");

        let srows = v["session_by_plan"]
            .as_array()
            .expect("session_by_plan is a list");
        let kimi = srows
            .iter()
            .find(|r| r["plan"] == "kimi")
            .expect("the session spent on kimi");
        assert_eq!(
            kimi["calls"], 1,
            "ONE call from this session, not the six the other session \
             and the global total both hold: {v:#?}"
        );
        // …while the global split still counts everything.
        let grows = v["by_plan"].as_array().expect("by_plan");
        let gkimi = grows.iter().find(|r| r["plan"] == "kimi").unwrap();
        assert_eq!(gkimi["calls"], 6, "1 + 5");
    }

    // ── the engine resolves the plan, once (2026-09-20) ──────────────

    /// The plan mapping lives in ONE place and this is it: the surface
    /// sends the provider and reads back the plan's name and fraction,
    /// instead of re-deriving it from a name substring — which is how
    /// the desktop and the engine came to disagree about `ali-responses`.
    #[test]
    fn an_unnamed_route_has_no_plan_and_a_real_one_is_named() {
        let mut snap = veles_core::tokenplan::QuotaSnapshot {
            fetched_at: chrono::Utc::now(),
            team: None,
            team_error: None,
            personal: None,
            personal_error: None,
            zai: None,
            zai_error: None,
            kimi: None,
            kimi_error: None,
        };
        snap.personal = Some(veles_core::tokenplan::PersonalQuota {
            windows: vec![veles_core::tokenplan::Window {
                label: "weekly".into(),
                used_fraction: Some(0.42),
                resets_at: None,
            }],
            free_tier: vec![],
        });

        let v = active_plan_json(&snap, "ali-token-plan-personal");
        assert_eq!(v["active_plan"], "personal");
        assert_eq!(v["active_plan_fraction"].as_f64(), Some(0.42));
        assert_eq!(v["plan_fractions"]["personal"].as_f64(), Some(0.42));
        assert!(v["plan_fractions"]["kimi"].is_null());

        // A provider on no plan: a name of null and NO fraction — not
        // another plan's number, and not the default route's.
        let v = active_plan_json(&snap, "openrouter");
        assert!(v["active_plan"].is_null(), "{v:#?}");
        assert!(
            v["active_plan_fraction"].is_null(),
            "no plan means no fraction, never a fallback: {v:#?}"
        );
        // …and every plan's own fraction is still served, so a widget
        // that lists all four does not have to ask four times.
        assert_eq!(v["plan_fractions"]["personal"].as_f64(), Some(0.42));
    }
}
