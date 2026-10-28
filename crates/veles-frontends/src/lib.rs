//! Frontends — the surfaces the agent talks through.
//!
//! Parity roadmap (hermes surfaces → veles frontends):
//!   CLI REPL            → frontends::cli          (this crate)
//!   gateway (telegram,
//!     discord, irc...)  → frontends::gateway core + per-platform adapters
//!                          (telegram = Rust plugin per USER decision)
//!   hermes desktop      → veles-desktop (Tcl/Tk app, separate package)
//!   Ink TUI            → frontends::tui (ratatui, USER-requested M1)
//!   hermes proxy        → proxy.rs (OpenAI-compatible, platforms.proxy)
//!   ACP server          → later
//!
//! Email and Signal adapters lived here as inbound-only stubs that
//! could never deliver a reply or an ask and were never started —
//! dead code pretending to be a feature, removed 2026-09-03. If they
//! come back they must register outbound like telegram and IRC do.

pub mod a2a_server;
pub mod chat;
pub mod cli;
pub mod commands;
pub mod desktop_api;
pub mod desktop_install;
pub mod desktop_join;
pub mod gateway;
pub mod panes;
pub mod tui;

// ── the agent handle the in-process surfaces share ───────────────────

use std::sync::{Arc, Mutex};
use veles_core::agent::Agent;
use veles_core::lockutil::{LockOk, RwLockOk};

/// The session handle the REPL and the TUI pass around.
///
/// **The lock is on the HANDLE, not the agent.** Switching sessions
/// swaps which `Arc<Agent>` it points at; every reader clones the
/// pointer out and drops the guard in the same breath. The agent itself
/// is shared immutably — `run_turn` takes `&self` — so a turn holds no
/// lock at all.
///
/// It used to be `Arc<Mutex<Agent>>`, with the turn holding that guard
/// across its whole `.await` behind an `#[allow(clippy::await_holding_lock)]`
/// in each of three surfaces. That is what froze the TUI on `/steer`:
/// its event loop is single-threaded, and the steer handler asked for
/// the guard the turn would not release until it had already finished —
/// so the steer landed, uselessly, after the turn it was meant to
/// redirect. Every command typed mid-turn had the same fate.
pub(crate) type AgentHandle = Arc<Mutex<Arc<Agent>>>;

/// The agent this handle currently points at.
///
/// Returns an owned `Arc` rather than a guard on purpose: a guard could
/// be held across an await, which is the hazard this type removes.
pub(crate) fn cur(h: &AgentHandle) -> Arc<Agent> {
    h.lock_ok().clone()
}

/// Wrap a freshly booted agent in a handle.
pub(crate) fn handle(agent: Agent) -> AgentHandle {
    Arc::new(Mutex::new(Arc::new(agent)))
}

/// Rebuild an agent pointing at a DIFFERENT session, carrying forward
/// everything the session did not choose — the surface, the plan path,
/// the asker, the principal, the `/model` overrides and the store.
///
/// This REBUILDS rather than assigning `session_id`, because the tool
/// registry is session-scoped: `todo`, `clarify` and `session_search`
/// are constructed with the session id baked in (`Agent::new` →
/// `builtin_registry(store, principal, sid)`). The REPL used to do
/// `a.session_id = id` for `/new` and `/resume`, which moved the
/// conversation and left every session-scoped tool still writing to the
/// session the user had just left.
///
/// `origin` is the delegate/cron provenance baked into the fresh tool
/// registry. Every in-process surface passes `None` (no caller); the
/// gateway passes its `platform:chat_id` so cron jobs created in this
/// session still deliver back to the chat that spawned them.
///
/// `cfg` is the config the fresh session is built on, and it is a
/// PARAMETER rather than a re-read because only the caller knows
/// whether `old` came from a file. The gateway knows (`cfg_from_file`,
/// captured at construction) and passes the live config, so `/new`
/// picks up a `[model]` edit. A surface that decided for itself by
/// asking `config::live()` got the wrong answer under test: the
/// file-or-memory question was answered from the AMBIANT environment,
/// so a test holding an in-memory config on a box that also has a real
/// `~/.hermes` built the forked agent out of the developer's actual
/// config. Three alias tests failed that way when this function tried
/// to be clever.
pub(crate) fn fork(
    old: &Arc<Agent>,
    cfg: Arc<veles_core::config::VelesConfig>,
    sid: &str,
    origin: Option<String>,
) -> anyhow::Result<Agent> {
    let registry = Arc::new(veles_core::tools::builtin_registry(
        old.store.clone(),
        origin,
        sid,
    ));
    let mut fresh = Agent::new(
        cfg,
        old.hooks.clone(),
        registry,
        old.store.clone(),
        sid.to_string(),
    )?;
    fresh.surface.clone_from(&old.surface);
    fresh.plan_path.clone_from(&old.plan_path);
    *fresh.overrides.write_ok() = old.overrides.read_ok().clone();
    if let Some(p) = old.principal() {
        fresh.set_principal((*p).clone());
    }
    if let Some(a) = old.asker() {
        // ATTACH on the fresh session id — the surface that owned the
        // asker keeps it alive; the old session's entry is not touched
        // (a fork must not steal its parent's route to the human, and
        // the reverse: detaching here never detached the parent's).
        fresh.set_asker(a);
    }
    Ok(fresh)
}

/// Point a handle at a different session.
///
/// This is [`fork`] plus the handle repoint: the same rebuild, stored
/// back into the `Arc<Mutex<Arc<Agent>>>` the in-process surfaces pass
/// around. The gateway keeps its own `sessions` map and calls [`fork`]
/// directly, because it is the one caller that has a live config to
/// hand — see `GatewayCore::session_cfg`.
///
/// The REPL keeps the config its agent was built on. It is a foreground
/// owner surface that restarts when the operator restarts it, and
/// deciding the file-or-memory question from the ambient environment
/// here — rather than from anything this process was told — is how three
/// gateway tests came to build a forked agent out of a scratch config
/// that did not exist.
pub(crate) fn rebind(h: &AgentHandle, sid: &str) -> anyhow::Result<Arc<Agent>> {
    let old = cur(h);
    let cfg = old.cfg.clone();
    let fresh = Arc::new(fork(&old, cfg, sid, None)?);
    *h.lock_ok() = fresh.clone();
    Ok(fresh)
}
