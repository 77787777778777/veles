//! Hook bus — mirrors hermes' plugin hook taxonomy so existing plugin
//! logic ports 1:1. Delivery consults registered RUST plugins first
//! (in-process, no hop — rustplugins.rs), then the Tcl thread (Tcl
//! plugins). A bus without a Tcl interpreter still reaches rust
//! handlers.
//!
//! Full hermes hook set (from plugins.py VALID_HOOKS), for parity:
//!   pre_tool_call, post_tool_call, transform_terminal_output,
//!   transform_tool_result, transform_llm_output, pre_llm_call,
//!   post_llm_call, pre_verify, pre_api_request, post_api_request,
//!   api_request_error, on_session_start, on_session_end,
//!   on_session_finalize, on_session_reset, subagent_start, subagent_stop,
//!   pre_gateway_dispatch, pre_approval_request, post_approval_response,
//!   kanban_task_claimed, kanban_task_completed, kanban_task_blocked.
//! Veles additions (S3/S11):
//!   compact_start — args: {session_id, history_chars, threshold_chars,
//!     cut}; handler may return {"action":"cancel"} to abort this
//!     compaction or {"extra_context": "..."} to re-inject text after
//!     the compaction marker.
//!   compact_end — args: {session_id, chars_before, chars_after, cut,
//!     archived}; informational.
//!   subagent_start — args: {child_id, goal}; fired by delegate.rs when
//!     a subagent child is registered.
//!   subagent_stop — args: {child_id, goal, ok}; fired when it finishes.
//!   background_child_complete — args: {child_id, goal, result}; fired
//!     by delegate.rs for a BACKGROUND child, alongside subagent_stop.
//!     All three were RESERVED and inert until 2026-09-03: the module
//!     doc claimed delegate.rs fired them and delegate.rs fired nothing.
//!   failover (S16, script mode) — args: {from_provider, from_model,
//!     error, session_id}; a handler returning {"action":"deny"} vetoes
//!     advancing to the next route (silence/other = advance).
//!   device_event (P4, PLAN-sessions-and-phone.md) — args: {device, kind,
//!     source, app, title, text, at}; fired by the desktop API's
//!     `POST /api/device/event` for each event a phone sends (kind
//!     notification: source is the posting app's package; kind power:
//!     source is connected|disconnected). Informational; the reply is
//!     ignored. The text is other people's words: data, never commands.

/// Every hook the engine can fire, as data rather than only prose.
///
/// The taxonomy above was a comment, so nothing could enumerate it: the
/// hooks manager can list the hooks something has REGISTERED for by
/// asking the interpreter, but not the ones still waiting for a
/// handler — and "which hooks exist" is exactly what someone writing a
/// plugin needs. Keep this in step with the doc comment above; the two
/// are checked against each other in the tests at the bottom of this
/// file.
pub const KNOWN_HOOKS: &[&str] = &[
    // hermes' VALID_HOOKS, in its order, for 1:1 plugin portability.
    "pre_tool_call",
    "post_tool_call",
    "transform_terminal_output",
    "transform_tool_result",
    "transform_llm_output",
    "pre_llm_call",
    "post_llm_call",
    "pre_verify",
    "pre_api_request",
    "post_api_request",
    "api_request_error",
    "on_session_start",
    "on_session_end",
    "on_session_finalize",
    "on_session_reset",
    "subagent_start",
    "subagent_stop",
    "pre_gateway_dispatch",
    "pre_approval_request",
    "post_approval_response",
    "kanban_task_claimed",
    "kanban_task_completed",
    "kanban_task_blocked",
    // veles additions (S3/S11/S16).
    "compact_start",
    "compact_end",
    "background_child_complete",
    "failover",
    "device_event",
];

/// Hooks the engine ACTUALLY emits, each with a `fire()` call site in
/// this tree.
///
/// `KNOWN_HOOKS` was a single flat list copied from hermes' VALID_HOOKS,
/// so a plugin author could register for `pre_verify` or
/// `kanban_task_claimed`, see it accepted, and wait forever for a call
/// that no code path makes. Splitting the list is the honest version:
/// these fire, the reserved ones are accepted for hermes portability and
/// documented as inert.
pub const FIRED_HOOKS: &[&str] = &[
    "pre_tool_call",
    "post_tool_call",
    "pre_llm_call",
    "post_llm_call",
    "api_request_error",
    "on_session_start",
    "pre_gateway_dispatch",
    "pre_approval_request",
    "post_approval_response",
    "compact_start",
    "compact_end",
    "failover",
    "subagent_start",
    "subagent_stop",
    "background_child_complete",
    "device_event",
];

/// Hooks accepted for hermes plugin portability that NOTHING in this
/// tree emits. Registering for one is legal and does nothing; the
/// taxonomy is kept so a ported plugin still loads rather than erroring
/// on an unknown hook name.
pub const RESERVED_HOOKS: &[&str] = &[
    "transform_terminal_output",
    "transform_tool_result",
    "transform_llm_output",
    "pre_verify",
    "pre_api_request",
    "post_api_request",
    "on_session_end",
    "on_session_finalize",
    "on_session_reset",
    "kanban_task_claimed",
    "kanban_task_completed",
    "kanban_task_blocked",
];

/// The hook names some plugin has registered a handler for, or `None`
/// when that is not known yet.
///
/// `fire()` costs a `spawn_blocking`, a oneshot channel and a Tcl eval
/// EVERY time, and `pre_llm_call` fires once per round while
/// `pre_tool_call`/`post_tool_call` fire once per tool call — so on a
/// machine with no plugins at all, a deep turn was paying dozens of
/// thread hops to be told "nobody is listening".
///
/// Refreshed by the Tcl thread itself at the only three moments the
/// registry can change: the initial plugin load, and both reload paths
/// (the explicit one and the file watcher, which converge on
/// `TclRequest::Reload`). `None` fires everything — an unknown registry
/// must never be a reason to silence a hook.
static REGISTERED: std::sync::OnceLock<
    std::sync::RwLock<Option<std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

fn registered() -> &'static std::sync::RwLock<Option<std::collections::HashSet<String>>> {
    REGISTERED.get_or_init(|| std::sync::RwLock::new(None))
}

/// Publish the current handler registry. Called from the Tcl thread
/// after every plugin (re)load — see `REGISTERED`.
pub fn set_registered_hooks<I: IntoIterator<Item = String>>(names: I) {
    let set: std::collections::HashSet<String> = names.into_iter().collect();
    if let Ok(mut g) = registered().write() {
        *g = Some(set);
    }
}

/// The process's hook bus, published so tools built by
/// `builtin_registry` can fire hooks without threading an
/// `Arc<HookBus>` through 26 call sites.
///
/// This mirrors `REGISTERED` above and is honest for the same reason: a
/// `HookBus` wraps one optional `TclHandle`, and there is one Tcl thread
/// per process, so a per-Agent bus and a per-process bus reach the same
/// handlers. `DelegateTaskTool` falls back to this when it was built
/// without an explicit bus.
static DEFAULT_BUS: std::sync::OnceLock<std::sync::RwLock<Option<Arc<HookBus>>>> =
    std::sync::OnceLock::new();

fn default_bus_cell() -> &'static std::sync::RwLock<Option<Arc<HookBus>>> {
    DEFAULT_BUS.get_or_init(|| std::sync::RwLock::new(None))
}

/// Publish the process hook bus. Called from `Agent::new`.
pub fn set_default_bus(bus: Arc<HookBus>) {
    if let Ok(mut g) = default_bus_cell().write() {
        *g = Some(bus);
    }
}

/// The process hook bus, if one has been published.
pub fn default_bus() -> Option<Arc<HookBus>> {
    default_bus_cell().read().ok().and_then(|g| g.clone())
}

/// Whether firing `name` can reach anything. Conservative: unknown
/// registry (or a poisoned lock) answers `true`. Counts BOTH halves:
/// rust plugin handlers (rustplugins.rs) and the Tcl registry.
pub fn has_handler(name: &str) -> bool {
    if crate::rustplugins::has_rust_handler(name) {
        return true;
    }
    tcl_has_handler(name)
}

/// The Tcl half of the gate — kept separate so `fire` can run the rust
/// path first and still skip the thread hop when no Tcl handler
/// listens.
fn tcl_has_handler(name: &str) -> bool {
    match registered().read() {
        Ok(g) => match &*g {
            None => true,
            Some(set) => set.contains(name),
        },
        Err(_) => true,
    }
}

/// The hook names rust plugins handle, published by
/// `rustplugins::register_plugin`. Kept here (not re-derived on every
/// gate check) because the gate runs on every hook fire.
static RUST_HOOK_NAMES: std::sync::OnceLock<std::sync::RwLock<Vec<String>>> =
    std::sync::OnceLock::new();

pub fn publish_rust_hook_names(names: Vec<String>) {
    let cell = RUST_HOOK_NAMES.get_or_init(|| std::sync::RwLock::new(Vec::new()));
    if let Ok(mut w) = cell.write() {
        *w = names;
    }
}

/// The published rust hook names (the read half of the gate).
pub fn rust_hook_names() -> Vec<String> {
    RUST_HOOK_NAMES
        .get_or_init(|| std::sync::RwLock::new(Vec::new()))
        .read()
        .map(|r| r.clone())
        .unwrap_or_default()
}

// ── which session is firing ──────────────────────────────────────────
//
// A hook payload is the ONLY channel from the engine to a handler (the
// `.so` ABI passes `(name, args_json)` and nothing else), so a handler
// that wanted to act — `veles::tool_call` most of all — could not say
// whose call it was making, and the bridge answered with the last
// session that happened to boot. Every fire site now stamps its
// session into the payload through here, and the two dispatch halves
// read it back out: `tclrt` into `::veles::hook_session` plus the
// ambient thread-local, `rustplugins::run_hooks` into the same
// thread-local. See `tools::plugin_call_registry` for how the answer is
// used.

/// Serialise `payload` with `session_id` stamped on it.
///
/// A `session_id` the payload already carries wins — the compaction and
/// approval hooks built theirs by hand before this existed, and a
/// stamping helper that overwrote a deliberate value would be a trap.
/// A non-object payload (no fire site produces one) is wrapped rather
/// than silently dropped, because a payload that reaches a handler
/// without its session is the bug this function exists to prevent.
pub fn with_session(session_id: &str, payload: serde_json::Value) -> String {
    match payload {
        serde_json::Value::Object(mut map) => {
            let present = map
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if !present {
                map.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
            serde_json::Value::Object(map).to_string()
        }
        other => {
            tracing::debug!("hook payload is not an object; wrapping it to carry session_id");
            serde_json::json!({ "session_id": session_id, "payload": other }).to_string()
        }
    }
}

/// The session a hook payload carries, when it carries one. Read by the
/// two dispatch halves to set the ambient identity for the handler call.
pub fn ambient_session(args_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args_json)
        .ok()?
        .get("session_id")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

use crate::tclrt::TclHandle;
use std::sync::Arc;

pub struct HookBus {
    tcl: Option<TclHandle>,
}

impl HookBus {
    pub fn new(tcl: Option<TclHandle>) -> Arc<Self> {
        Arc::new(Self { tcl })
    }

    /// Fire a hook. Returns a JSON string a handler produced (or None).
    ///
    /// Order: RUST handlers first (in-process, sync, first non-empty
    /// verdict wins — hermes semantics), then the Tcl plugins via the
    /// thread hop. A bus without a Tcl interpreter still reaches the
    /// rust half.
    pub async fn fire(&self, name: &str, args_json: &str) -> Option<String> {
        // Rust handlers are SYNC by contract, and the contract says the
        // engine runs them somewhere blocking is allowed. It did not:
        // until 2026-09-04 this line called `run_hooks` straight from
        // the async fn, on an executor thread. A handler that took its
        // time stalled the runtime, and one that blocked on anything
        // wedged it — which became load-bearing the moment the plugin
        // ABI grew `host_call`, since every engine op blocks on the
        // bridge's runtime and tokio panics (not errors) when you block
        // inside an async context.
        //
        // Guarded by `has_rust_handler` so the ordinary no-plugin fire
        // costs a name lookup rather than a thread hop, the same way
        // `tcl_has_handler` guards the Tcl side below.
        if crate::rustplugins::has_rust_handler(name) {
            let (n, a) = (name.to_string(), args_json.to_string());
            let verdict = crate::rt::spawn_blocking(move || crate::rustplugins::run_hooks(&n, &a))
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("rust hook handler panicked or was cancelled: {e}");
                    None
                });
            if verdict.is_some() {
                return verdict;
            }
        }
        // Nobody registered for this one: skip the thread hop entirely.
        // See REGISTERED — this is conservative, so a not-yet-known
        // registry still fires.
        if !tcl_has_handler(name) {
            return None;
        }
        let tcl = self.tcl.clone()?;
        let name = name.to_string();
        let args = args_json.to_string();
        // The Tcl thread is sync; hop via spawn_blocking.
        crate::rt::spawn_blocking(move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tcl.send(crate::tclrt::TclRequest::Hook {
                name,
                args_json: args,
                reply: tx,
            });
            match rx.blocking_recv() {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    tracing::warn!("hook error: {e}");
                    None
                }
                Err(_) => None,
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Run a NAMED failover script: source its file and call its
    /// `failover::handle`. A missing/broken script or a missing proc comes
    /// back as a deny verdict (the route stops) rather than a silent
    /// advance — a failover policy you wrote must not silently do nothing.
    pub async fn fire_script(&self, script_file: &str, args_json: &str) -> Option<String> {
        let Some(tcl) = self.tcl.clone() else {
            return Some(
                serde_json::json!({
                    "action": "deny",
                    "reason": "no Tcl interpreter (gateway has no plugin runtime)"
                })
                .to_string(),
            );
        };
        let script_file = script_file.to_string();
        let args = args_json.to_string();
        crate::rt::spawn_blocking(move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tcl.send(crate::tclrt::TclRequest::ScriptHook {
                script_file,
                args_json: args,
                reply: tx,
            });
            match rx.blocking_recv() {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => Some(
                    serde_json::json!({
                        "action": "deny",
                        "reason": format!("failover script failed: {e}")
                    })
                    .to_string(),
                ),
                Err(_) => None,
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Is there a Tcl interpreter behind this bus at all?
    ///
    /// The difference between "your script is broken" and "this process
    /// cannot run scripts" matters to anything that saves one: the
    /// first is a bad edit to reject, the second is a gateway built
    /// without a plugin runtime, where refusing the write would simply
    /// lose the user's work.
    pub fn has_tcl(&self) -> bool {
        self.tcl.is_some()
    }

    /// Evaluate Tcl in the live interpreter (REPL / debugger / hot-patch
    /// surface). Loopback-only callers. Returns Ok(result) or Err(msg).
    pub async fn eval_tcl(&self, code: &str) -> Result<String, String> {
        let Some(tcl) = self.tcl.clone() else {
            return Err("no Tcl interpreter (gateway has no plugin runtime)".into());
        };
        let code = code.to_string();
        crate::rt::spawn_blocking(move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tcl.send(crate::tclrt::TclRequest::Eval { code, reply: tx });
            match rx.blocking_recv() {
                Ok(res) => res,
                Err(_) => Err("tcl thread dropped the eval".to_string()),
            }
        })
        .await
        .map_err(|e| format!("eval join failed: {e}"))?
    }

    /// Re-source every discovered plugin script (the hot-reload path,
    /// triggered manually — the fs watcher does the automatic one).
    pub async fn reload_plugins(&self) -> Result<(), String> {
        let Some(tcl) = self.tcl.clone() else {
            return Err("no Tcl interpreter".into());
        };
        crate::rt::spawn_blocking(move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tcl.send(crate::tclrt::TclRequest::Reload { reply: tx });
            match rx.blocking_recv() {
                Ok(res) => res,
                Err(_) => Err("tcl thread dropped the reload".to_string()),
            }
        })
        .await
        .map_err(|e| format!("reload join failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The taxonomy is stated twice — once as prose at the top of this
    /// file, once as `KNOWN_HOOKS` — and a plugin author reads the
    /// prose. Check the constant against the comment so the two cannot
    /// drift; the failure names the missing hook rather than just
    /// counting.
    #[test]
    fn known_hooks_matches_the_documented_taxonomy() {
        let source = include_str!("hooks.rs");
        let docs: String = source
            .lines()
            .take_while(|l| l.starts_with("//!"))
            .collect::<Vec<_>>()
            .join("\n");
        for hook in KNOWN_HOOKS {
            assert!(
                docs.contains(hook),
                "`{hook}` is in KNOWN_HOOKS but not in the module doc comment"
            );
        }
    }

    #[test]
    fn known_hooks_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for h in KNOWN_HOOKS {
            assert!(seen.insert(*h), "`{h}` is listed twice");
        }
    }

    /// The hook the whole plugin system is demonstrated with, and the
    /// one the scaffold registers, must be spelled the same everywhere.
    #[test]
    fn the_scaffolds_hook_is_a_real_one() {
        assert!(KNOWN_HOOKS.contains(&"post_llm_call"));
    }

    // ── the fired/reserved split (Step 4.1) ─────────────────────────

    /// The split must PARTITION the taxonomy: every known hook is
    /// either fired or reserved, never both and never neither. A hook
    /// that falls out of both lists is one nobody can reason about.
    #[test]
    fn fired_and_reserved_partition_known_hooks() {
        let fired: std::collections::HashSet<_> = FIRED_HOOKS.iter().collect();
        let reserved: std::collections::HashSet<_> = RESERVED_HOOKS.iter().collect();
        for h in KNOWN_HOOKS {
            assert!(
                fired.contains(h) != reserved.contains(h),
                "`{h}` must be in exactly one of FIRED_HOOKS / RESERVED_HOOKS"
            );
        }
        assert_eq!(
            FIRED_HOOKS.len() + RESERVED_HOOKS.len(),
            KNOWN_HOOKS.len(),
            "the split must cover KNOWN_HOOKS exactly"
        );
        for h in FIRED_HOOKS.iter().chain(RESERVED_HOOKS.iter()) {
            assert!(
                KNOWN_HOOKS.contains(h),
                "`{h}` is in the split but not in KNOWN_HOOKS"
            );
        }
    }

    /// `FIRED_HOOKS` is a claim about this tree, so check it against the
    /// tree: every name in it must have a real `fire("<name>"` call
    /// site. Without this the list is just a second comment to rot —
    /// and the whole point of the split is that it cannot.
    ///
    /// Sources are included at compile time; the search is textual on
    /// purpose, matching what a reader greps for.
    /// Every `.rs` under this crate's `src/`, WALKED rather than
    /// hand-listed.
    ///
    /// The two tests below used a hardcoded three-file list —
    /// agent.rs, delegate.rs, gateway.rs — which was true when it was
    /// written and is a promise nobody renews. Fire a RESERVED hook
    /// from cron.rs or monitor.rs and `no_reserved_hook_has_a_call_site`
    /// passes while the docs call the hook inert.
    ///
    /// Same shape as the fileset omission found in flake.nix on
    /// 2026-09-04: a scan is only as honest as its coverage, and a
    /// hand-listed coverage is one refactor from being wrong. Walking
    /// costs nothing and cannot go stale.
    ///
    /// hooks.rs itself is excluded — it defines the constants, so every
    /// name appears in it by construction.
    ///
    /// So is each file's trailing `#[cfg(test)] mod …` block: these scans
    /// are claims about what the ENGINE fires, and a fire inside a test is
    /// a fixture rather than a call site (one that legitimately has no
    /// session to stamp). See `production_only`.
    fn hook_call_sites() -> String {
        fn walk(dir: &std::path::Path, out: &mut String) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.file_name().is_some_and(|f| f == "hooks.rs") {
                    continue;
                } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        out.push_str(&production_only(&s));
                        out.push('\n');
                    }
                }
            }
        }
        /// Everything before a file's `#[cfg(test)] mod …` block.
        ///
        /// The marker only starts a test module when the next non-empty
        /// line opens a `mod`, so the `#[cfg(test)] pub fn
        /// register_turn_for_test` helpers (agent.rs) and the
        /// `#[cfg(test)] impl` blocks stay in — they are production-adjacent
        /// code a scan should still see.
        ///
        /// Assumes test modules are TRAILING, which is the convention
        /// across this tree. A file that put production code after one
        /// would be cut short, and `every_fired_hook_has_a_call_site`
        /// would fail loudly the moment that code fired a hook — the
        /// failure mode is a red test, not a silent pass.
        fn production_only(src: &str) -> String {
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.trim_start().starts_with("#[cfg(test)]") {
                    continue;
                }
                let opens_mod = lines[i + 1..]
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.trim_start().starts_with("mod "));
                if opens_mod {
                    return lines[..i].join("\n");
                }
            }
            src.to_string()
        }
        let mut out = String::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        // The surfaces are their own crate since 2026-09-28 (veles-frontends);
        // they are engine sources too.
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../veles-frontends/src"),
            &mut out,
        );
        // A walk that found nothing would make every assertion below
        // pass vacuously — the exact failure mode guards.rs guards
        // against with the same one-line check.
        assert!(
            out.contains(".fire("),
            "the walk found no hook call sites at all — the path is wrong, and \
             every assertion built on this would pass for the wrong reason"
        );
        out
    }

    #[test]
    fn every_fired_hook_has_a_call_site() {
        let sources = hook_call_sites();
        for h in FIRED_HOOKS {
            let needle = format!("\"{h}\"");
            assert!(
                sources.contains(&needle),
                "`{h}` is in FIRED_HOOKS but nothing fires it — move it to RESERVED_HOOKS"
            );
        }
    }

    /// The reverse drift: a hook listed as RESERVED that someone has
    /// since wired up is now lying in the other direction, and — worse
    /// — `has_handler` would be gating a hook the docs call inert.
    /// This is how `background_child_complete` was caught: the module
    /// doc claimed delegate.rs fired it, and delegate.rs fires nothing.
    #[test]
    fn no_reserved_hook_has_a_call_site() {
        let sources = hook_call_sites();
        for h in RESERVED_HOOKS {
            let needle = format!(".fire(\"{h}\"");
            assert!(
                !sources.contains(&needle),
                "`{h}` is fired somewhere but listed RESERVED — move it to FIRED_HOOKS"
            );
        }
    }

    /// The short-circuit must be CONSERVATIVE: before the Tcl thread
    /// has published a registry, every hook still fires. A cache that
    /// defaulted to "empty" would silently disable every plugin during
    /// startup, which is worse than the thread hop it saves.
    #[test]
    fn an_unknown_registry_fires_everything() {
        let _g = registry_gate();
        // Fresh process state (or another test's publish) — either way,
        // asking about a hook must never panic, and with no registry
        // published the answer is yes.
        if registered().read().unwrap().is_none() {
            assert!(has_handler("pre_llm_call"));
            assert!(has_handler("anything_at_all"));
        }
    }

    /// The registry tests check-then-assert on process-global state:
    /// `an_unknown_registry…` reads `is_none()` then asks `has_handler`,
    /// and `a_published_registry…` publishes in between — a real
    /// interleaving that fired once on 2026-09-07 when a new reader
    /// (the /hooks command test) shifted test timing. One mutex, both
    /// sides; readers elsewhere stay lock-free.
    fn registry_gate() -> &'static std::sync::Mutex<()> {
        static G: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        G.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// Once a registry IS published, only its members fire.
    #[test]
    fn a_published_registry_gates_the_hop() {
        let _g = registry_gate();
        set_registered_hooks(vec!["post_llm_call".to_string()]);
        assert!(has_handler("post_llm_call"), "registered hooks fire");
        assert!(
            !has_handler("pre_tool_call"),
            "an unhandled hook skips the thread hop"
        );
        // Restore the conservative default so test order cannot matter.
        if let Ok(mut g) = registered().write() {
            *g = None;
        }
    }

    // ── the session a payload carries ────────────────────────────────

    /// Every payload reaches its handler with the session that fired it,
    /// because that is the only way a handler can act as its own caller
    /// (`tools::plugin_call_registry` reads it back). A session the
    /// payload already names WINS: three hooks built theirs by hand
    /// before the stamping helper existed, and a helper that overwrote a
    /// deliberate value would be a trap.
    #[test]
    fn a_payload_carries_its_session_and_a_deliberate_one_wins() {
        let stamped = with_session("s1", serde_json::json!({"tool": "terminal"}));
        assert_eq!(ambient_session(&stamped).as_deref(), Some("s1"));
        assert!(
            stamped.contains("\"tool\":\"terminal\""),
            "the payload itself must survive the stamping: {stamped}"
        );

        let kept = with_session("s2", serde_json::json!({"session_id": "s1"}));
        assert_eq!(
            ambient_session(&kept).as_deref(),
            Some("s1"),
            "an explicit session_id in the payload must not be overwritten"
        );

        // Absent, not a session named "".
        assert_eq!(
            ambient_session(&with_session("", serde_json::json!({}))),
            None
        );
        assert_eq!(ambient_session("{}"), None);
        assert_eq!(ambient_session("not json"), None);
        assert_eq!(ambient_session(r#"{"session_id":""}"#), None);
        assert_eq!(ambient_session(r#"{"session_id":42}"#), None);

        // A non-object payload is wrapped rather than silently losing
        // its session — no fire site produces one, and that is the point
        // of making it total.
        let wrapped = with_session("s3", serde_json::json!(3));
        assert_eq!(ambient_session(&wrapped).as_deref(), Some("s3"));
    }

    /// Every hook the engine ACTUALLY fires must be stamped by its call
    /// site. `FIRED_HOOKS` is already checked against the tree for a
    /// `fire("<name>"` spelling; this checks the other half of the
    /// promise — that the payload carries the session — so a new fire
    /// site cannot quietly reintroduce "judged as whoever booted last".
    #[test]
    fn every_fired_hook_stamps_its_session_at_the_call_site() {
        // Whitespace-stripped, because rustfmt puts `.fire(` and the hook
        // name on separate lines whenever the payload is more than a
        // one-liner — and that is every site worth checking. The needles
        // below contain no spaces, so the strip cannot hide one.
        let sources: String = hook_call_sites().split_whitespace().collect();
        for h in FIRED_HOOKS {
            let needle = format!(".fire(\"{h}\"");
            let sites: Vec<usize> = sources.match_indices(&needle).map(|(i, _)| i).collect();
            assert!(
                !sites.is_empty(),
                "`{h}` is in FIRED_HOOKS but has no call site — the scan above should have \
                 caught this first"
            );
            for at in sites {
                // One call: from its `.fire(` to the `.await` that ends
                // it. EVERY site must stamp, not just one — a hook fired
                // from two places (subagent_start/stop are) is only as
                // honest as the site that forgot.
                let tail = &sources[at..];
                let window = match tail.find(".await") {
                    Some(end) => &tail[..end],
                    None => tail,
                };
                // The stamping helper is the sanctioned way in; a site
                // that builds its payload by hand and already carries the
                // key counts too (compact_start/compact_end/
                // pre_approval_request did that before the helper
                // existed, and `with_session` deliberately keeps a
                // session the payload already names).
                assert!(
                    window.contains("with_session") || window.contains("\"session_id\""),
                    "`{h}` fires without stamping its session: a handler calling \
                     veles::tool_call from it would be judged as whichever session published a \
                     registry last. Wrap the payload in hooks::with_session(). \
                     Site (whitespace stripped):\n{window}"
                );
            }
        }
    }
}
