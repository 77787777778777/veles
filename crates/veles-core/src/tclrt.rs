//! Tcl runtime — the heart of the "bidirectional, tightly connected" design.
//!
//! Unlike slopdrop (Tcl as an eval-sandbox for chat commands), veles makes
//! Tcl a first-class citizen of the agent loop:
//!
//!   Rust → Tcl:  every lifecycle event is a Tcl command invocation
//!                (hooks, tool results, incoming messages).
//!   Tcl  → Rust: Tcl commands registered by the core (`veles::call_llm`,
//!                `veles::tool`, `veles::emit`, sqlite access, HTTP) let
//!                plugin code drive the whole engine.
//!
//! One interpreter per *runtime domain*:
//!   - `plugins` interp: long-lived, hosts plugin procs, git-backed state
//!     (slopdrop pattern) for live-editable definitions.
//!   - `aux` interps: short-lived sandboxes for untrusted eval (parity with
//!     slopdrop's SafeTclInterp: no exec/file/socket, rlimit, timeout).
//!
//! The interpreter is !Send (Tcl limitation), so it lives on a dedicated
//! thread with an mpsc request channel — exactly slopdrop's TclThreadHandle
//! model, extended with async completion on the tokio side.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use tokio::sync::oneshot;

/// A request into the Tcl thread.
#[derive(Debug)]
pub enum TclRequest {
    /// Evaluate code in the plugins interpreter (trusted).
    Eval {
        code: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Set a variable VERBATIM (interp.set — arbitrary content, JSON
    /// included, never word-quoting), then evaluate code. The only safe
    /// way to pass structured data through one hop: Tcl brace quoting
    /// does not round-trip JSON (backslash-braces stay literal inside
    /// braced words).
    SetEval {
        var: String,
        value: String,
        code: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Fire a hook: run every proc registered for this hook name.
    Hook {
        name: String,
        args_json: String,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Run a NAMED failover script: source its file, set hook_args, and
    /// call the `failover::handle` proc it defines. Returns the proc's
    /// result ("" = advance, {"action":"deny"} = veto).
    ScriptHook {
        script_file: String,
        args_json: String,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Reload all plugin scripts (hot reload).
    Reload {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Unload ONE plugin by name (R7): its hook handlers leave the
    /// interpreter's registry, what it registered in core leaves by
    /// owner (`tcl:<name>`), and every load — the watcher's included —
    /// skips it until `Load` names it again.
    Unload {
        name: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Source ONE plugin again, purging what it had first: `start` for
    /// an unloaded plugin, a per-plugin reload for a loaded one.
    Load {
        name: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// The Tcl plugins switched off at runtime (`Unload`), by name. Not
/// config: `[plugins] disabled` is the durable switch, this is the
/// running gateway's, and a restart forgets it.
fn unloaded() -> &'static std::sync::Mutex<std::collections::BTreeSet<String>> {
    static UNLOADED: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    UNLOADED.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()))
}

fn is_unloaded(name: &str) -> bool {
    unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(name)
}

/// The owner tag a Tcl plugin's registrations carry.
pub fn owner_tag_for(name: &str) -> &'static str {
    crate::lifecycle::owner_tag(&format!("tcl:{name}"))
}

/// One row per enabled Tcl plugin for the plugins listing:
/// `(name, state)` with state `loaded` or `unloaded`.
pub fn plugin_rows() -> Vec<(String, &'static str)> {
    let cfg = crate::config::load().unwrap_or_default();
    crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.manifest.kind == "tcl")
        .map(|p| {
            let state = if is_unloaded(&p.manifest.name) {
                "unloaded"
            } else {
                "loaded"
            };
            (p.manifest.name, state)
        })
        .collect()
}

pub struct TclHandle {
    tx: mpsc::Sender<TclRequest>,
}

/// The plugins interpreter's handle, parked process-wide at spawn.
/// Sync surfaces needing a Tcl hop (`.md`-style Tcl command rendering,
/// the bridge's `register_tool` adapter) resolve it here; async callers
/// must send an Eval request and AWAIT the oneshot — never
/// blocking_recv inside a runtime (the 2026-08-22 incident).
static GLOBAL_HANDLE: std::sync::OnceLock<std::sync::RwLock<Option<TclHandle>>> =
    std::sync::OnceLock::new();

fn global_handle_cell() -> &'static std::sync::RwLock<Option<TclHandle>> {
    GLOBAL_HANDLE.get_or_init(|| std::sync::RwLock::new(None))
}

pub fn set_global_handle(handle: TclHandle) {
    if let Ok(mut w) = global_handle_cell().write() {
        *w = Some(handle);
    }
}

pub fn global_handle() -> Option<TclHandle> {
    global_handle_cell().read().ok()?.clone()
}

impl Clone for TclHandle {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl TclHandle {
    /// Send a raw request to the Tcl thread (used by the hook bus).
    pub fn send(&self, req: TclRequest) -> Result<(), mpsc::SendError<TclRequest>> {
        self.tx.send(req)
    }

    /// Unload one Tcl plugin by name (R7). Async: hops to the Tcl
    /// thread through the blocking pool, like `HookBus::eval_tcl`.
    pub async fn unload_plugin(&self, name: &str) -> Result<String, String> {
        let name = name.to_string();
        self.hop(move |reply| TclRequest::Unload { name, reply })
            .await
    }

    /// Source one Tcl plugin (again): `start` after an unload, or a
    /// per-plugin reload.
    pub async fn load_plugin(&self, name: &str) -> Result<String, String> {
        let name = name.to_string();
        self.hop(move |reply| TclRequest::Load { name, reply })
            .await
    }

    async fn hop(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<String, String>>) -> TclRequest + Send + 'static,
    ) -> Result<String, String> {
        let tx = self.tx.clone();
        crate::rt::spawn_blocking(move || {
            let (reply, rx) = oneshot::channel();
            tx.send(make(reply))
                .map_err(|_| "tcl thread dead".to_string())?;
            rx.blocking_recv()
                .map_err(|_| "tcl thread dropped the request".to_string())?
        })
        .await
        .map_err(|e| format!("join failed: {e}"))?
    }

    /// Blocking eval — callers are off the hot async path (CLI, doctor).
    ///
    /// Refuses rather than panics when called from inside a tokio
    /// runtime. `blocking_recv()` there panics with "Cannot block the
    /// current thread from within a runtime", and that panic is exactly
    /// what poisoned the store mutex and took the gateway down on
    /// 2026-08-22. Async callers want `HookBus::eval_tcl`, which hops
    /// through `spawn_blocking`.
    pub fn eval(&self, code: &str) -> Result<String, String> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(
                "TclHandle::eval called from async context — use HookBus::eval_tcl instead"
                    .to_string(),
            );
        }
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TclRequest::Eval {
                code: code.to_string(),
                reply: tx,
            })
            .map_err(|_| "tcl thread dead".to_string())?;
        rx.blocking_recv()
            .map_err(|_| "tcl thread dropped".to_string())?
    }

    /// Blocking set-then-eval — same runtime guard as `eval`, same
    /// contract as `TclRequest::SetEval` (the value lands verbatim).
    pub fn set_eval(&self, var: &str, value: &str, code: &str) -> Result<String, String> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(
                "TclHandle::set_eval called from async context — send TclRequest::SetEval \
                 and await the reply instead"
                    .to_string(),
            );
        }
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TclRequest::SetEval {
                var: var.to_string(),
                value: value.to_string(),
                code: code.to_string(),
                reply: tx,
            })
            .map_err(|_| "tcl thread dead".to_string())?;
        rx.blocking_recv()
            .map_err(|_| "tcl thread dropped".to_string())?
    }

    /// Block until the interpreter has finished starting up.
    ///
    /// `spawn` returns as soon as the thread is created; the thread
    /// then registers commands, builds the bridge and LOADS PLUGINS
    /// before it starts serving requests. So anything that inspects
    /// what plugins registered — a hook registry, a tool, a provider
    /// driver — sees an empty world if it looks straight after
    /// `spawn`.
    ///
    /// That is not hypothetical: `veles doctor --providers` reported
    /// "no drivers registered" on a machine with a driver plugin,
    /// because it asked in the microseconds before the plugin loaded.
    ///
    /// The mechanism is the request queue, which is FIFO and only
    /// drained after initialisation, so one completed round trip means
    /// everything before it has happened. There is nothing to poll and
    /// no timeout to tune.
    pub fn wait_ready(&self) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TclRequest::Eval {
                code: "set ::veles_ready 1".to_string(),
                reply: tx,
            })
            .map_err(|_| "tcl thread dead".to_string())?;
        rx.blocking_recv()
            .map_err(|_| "tcl thread dropped".to_string())?
            .map(|_| ())
    }

    /// `set_eval` without the async-context refusal, for a caller that
    /// KNOWS it is on a blocking thread.
    ///
    /// The guard above cannot tell the two cases apart. Tokio reports a
    /// current runtime handle on `spawn_blocking` threads as well as on
    /// executor threads, and blocking there is not only legal but the
    /// entire point — so a guard that refuses when a handle exists
    /// refuses the correct callers too. (The plugin ABI's `host_call`
    /// hit the same wall; see rustplugins.)
    ///
    /// `set_eval` keeps its guard, because its callers are the REPL,
    /// the TUI and chat, where a stray async caller is a real mistake
    /// and the failure it prevents is the 2026-08-22 gateway wedge.
    /// This one exists for provider drivers, which are invoked from
    /// `spawn_blocking` by construction.
    pub fn set_eval_from_blocking_thread(
        &self,
        var: &str,
        value: &str,
        code: &str,
    ) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TclRequest::SetEval {
                var: var.to_string(),
                value: value.to_string(),
                code: code.to_string(),
                reply: tx,
            })
            .map_err(|_| "tcl thread dead".to_string())?;
        rx.blocking_recv()
            .map_err(|_| "tcl thread dropped".to_string())?
    }
}

/// Spawn the Tcl thread. Registers core bridge commands before sourcing
/// any plugin script.
pub fn spawn(home: PathBuf) -> Result<TclHandle> {
    // `home` supplies THREE plugin-directory derivations below
    // (sync_api_stub, git_snapshot_plugins, start_plugin_watcher) while
    // the loader itself now uses `plugins::plugins_dir()`. Every real
    // caller passes `config_dir()`, so the two agree — but when they
    // stop agreeing the failure is silent and awful: the watcher
    // watches one tree, the loader sources another, and edits appear to
    // do nothing. That divergence is exactly what the XDG split did to
    // the five frontend callers of `discover` (see plugins.rs), so it
    // gets a warning rather than a comment hoping nobody does it.
    let want = crate::plugins::plugins_dir();
    if home.join("plugins") != want {
        tracing::warn!(
            "tcl runtime spawned with home {} (plugins would be {}) but the loader \
             reads {} — hot reload and the api stub will target the wrong tree",
            home.display(),
            home.join("plugins").display(),
            want.display()
        );
    }
    let (tx, rx) = mpsc::channel::<TclRequest>();
    let watch_tx = tx.clone(); // for the plugin watcher (tx moves below)

    thread::Builder::new()
        .name("veles-tcl".into())
        .spawn(move || {
            let mut interp = match tcl::Interpreter::new() {
                Ok(i) => i,
                Err(e) => {
                    tracing::error!("tcl init failed: {e}");
                    return;
                }
            };
            register_bridge_commands(&mut interp, home.clone());

            // Native FFI bridge: real engine capabilities as Tcl
            // commands. Built whenever config loads — routes resolve
            // lazily per call, so a provider-less boot no longer costs
            // plugins the whole bridge (sessions/history/http/tools
            // all work without any route configured).
            if let Some(bridge) = crate::bridge::Bridge::new() {
                // Published BEFORE register, which consumes the Arc
                // (Arc::into_raw into ClientData). This is the bridge
                // out-of-tree `.so` plugins reach through the ABI's
                // host_call, which has no ClientData channel of its own.
                crate::bridge::publish_global(std::sync::Arc::clone(&bridge));
                unsafe { bridge.register(&interp) };
                tracing::info!("native bridge registered");
            } else {
                tracing::warn!("native bridge unavailable (config unreadable)");
            }

            if let Err(e) = load_plugins(&mut interp) {
                tracing::error!("plugin load failed: {e}");
            }
            publish_hook_registry(&mut interp);

            // Keep the tooling stub beside the live plugins (see
            // sync_api_stub). Before the watcher starts, so the write is
            // never a reload trigger.
            sync_api_stub(&home);

            // Git-backed plugin state (slopdrop pattern): every (re)load
            // that changes plugin files gets a commit, enabling rollback.
            git_snapshot_plugins(&home);

            // Hot reload: watch the plugins dir, re-source on change.
            start_plugin_watcher(home.clone(), watch_tx);
            while let Ok(req) = rx.recv() {
                match req {
                    TclRequest::Eval { code, reply } => {
                        let res = interp
                            .eval(code.as_str())
                            .map(|v| v.to_string())
                            .map_err(|e| e.to_string());
                        let _ = reply.send(res);
                    }
                    TclRequest::SetEval {
                        var,
                        value,
                        code,
                        reply,
                    } => {
                        // interp.set is the verbatim transport — JSON
                        // braces survive exactly as the Hook arm does it.
                        interp.set(var.as_str(), value);
                        let res = interp
                            .eval(code.as_str())
                            .map(|v| v.to_string())
                            .map_err(|e| e.to_string());
                        let _ = reply.send(res);
                    }
                    TclRequest::Hook {
                        name,
                        args_json,
                        reply,
                    } => {
                        // Transport: JSON goes into ::veles::hook_args via
                        // interp.set() — never through Tcl word quoting, so
                        // braces/brackets in the JSON survive intact.
                        interp.set("::veles::hook_args", args_json.clone());
                        // The session this hook fires for, both ways a
                        // handler can reach it: `::veles::hook_session` for
                        // a proc that wants to name it explicitly (one that
                        // defers its callback to a thread of its own, where
                        // the ambient below does not follow), and the
                        // ambient identity `veles::tool_call` reads when the
                        // plugin names no session — so a call made from this
                        // hook is judged as this hook's caller rather than
                        // as whichever session booted last.
                        //
                        // Both are scoped to this dispatch and cleared
                        // after it, error or panic notwithstanding (the
                        // guard in `with_ambient_hook_session` is a Drop):
                        // a stale ambient would judge the next caller on
                        // this thread as this session, which is the same
                        // class of mistake in the other direction.
                        let sid = crate::hooks::ambient_session(&args_json);
                        interp.set("::veles::hook_session", sid.as_deref().unwrap_or(""));
                        let code = format!(
                            "if {{[info commands ::veles::dispatch_hook] ne \"\"}} {{ \
                             ::veles::dispatch_hook {} }} else {{ return \"\" }}",
                            quote(&name)
                        );
                        let res = crate::tools::with_ambient_hook_session(sid.as_deref(), || {
                            interp
                                .eval(code.as_str())
                                .map(|v| {
                                    let s = v.to_string();
                                    if s.is_empty() {
                                        None
                                    } else {
                                        Some(s)
                                    }
                                })
                                .map_err(|e| e.to_string())
                        });
                        interp.set("::veles::hook_session", "");
                        let _ = reply.send(res);
                    }
                    TclRequest::ScriptHook {
                        script_file,
                        args_json,
                        reply,
                    } => {
                        // Same safe transport as Hook: args via interp.set,
                        // never through Tcl word quoting — braces/brackets
                        // in the JSON survive intact. Same session scoping
                        // too: a failover script is a hook handler with a
                        // name, and one that calls `veles::tool_call` must
                        // be judged as the session whose route failed.
                        interp.set("::veles::hook_args", args_json.clone());
                        let sid = crate::hooks::ambient_session(&args_json);
                        interp.set("::veles::hook_session", sid.as_deref().unwrap_or(""));
                        let code = format!(
                            "if {{[file exists {}]}} {{ source {} }}\n\
                             if {{[info commands failover::handle] eq \"\"}} {{\n\
                               return -code error \"no failover::handle proc in {}\"\n\
                             }}\n\
                             failover::handle",
                            quote(&script_file),
                            quote(&script_file),
                            quote(&script_file)
                        );
                        let res = crate::tools::with_ambient_hook_session(sid.as_deref(), || {
                            interp
                                .eval(code.as_str())
                                .map(|v| {
                                    let s = v.to_string();
                                    if s.is_empty() {
                                        None
                                    } else {
                                        Some(s)
                                    }
                                })
                                .map_err(|e| e.to_string())
                        });
                        interp.set("::veles::hook_session", "");
                        let _ = reply.send(res);
                    }
                    TclRequest::Reload { reply } => {
                        // Reset the hook registry first, or re-sourcing
                        // plugins would double-register their handlers.
                        let _ = interp.eval(
                            "array unset ::veles::hooks; array set ::veles::hooks {}; \
                             array unset ::veles::hook_owner; array set ::veles::hook_owner {}",
                        );
                        let res = load_plugins(&mut interp).map_err(|e| e.to_string());
                        publish_hook_registry(&mut interp);
                        let _ = reply.send(res);
                    }
                    TclRequest::Unload { name, reply } => {
                        let _ = reply.send(unload_plugin(&mut interp, &name));
                    }
                    TclRequest::Load { name, reply } => {
                        let _ = reply.send(load_one(&mut interp, &name));
                    }
                }
            }
        })?;

    let handle = TclHandle { tx };
    // Park it process-wide BEFORE returning so anything the boot path
    // still does (adapter registration, bridge callbacks) can hop.
    set_global_handle(handle.clone());
    Ok(handle)
}

/// Publish the interpreter's hook registry to the Rust side so
/// `HookBus::fire` can skip the thread hop for hooks nobody handles.
///
/// Called from the Tcl thread ONLY, at every point the registry can
/// change: the initial `load_plugins` and the `Reload` request (which
/// both the explicit reload and the file watcher go through). Doing it
/// here rather than lazily on first fire is what keeps the cache from
/// going stale after a hot reload — the refresh happens on the same
/// thread that performed the change, immediately after it.
fn publish_hook_registry(interp: &mut tcl::Interpreter) {
    match interp.eval("lsort [array names ::veles::hooks]") {
        Ok(v) => {
            let names = v.to_string();
            crate::hooks::set_registered_hooks(
                names
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            );
        }
        // Leave the cache as it was; a failed read must not be
        // mistaken for "no handlers registered".
        Err(e) => tracing::warn!("could not read hook registry: {e}"),
    }
}

pub(crate) fn quote(s: &str) -> String {
    // Tcl list-element quoting.
    format!("{{{}}}", s.replace('{', "\\{").replace('}', "\\}"))
}

/// Push the persisted hook off-list into the interpreter.
///
/// The interpreter is the authority on what RUNS; config.toml is the
/// authority on what the user chose. This is the one place the second
/// is copied into the first, and it clears first so a row removed from
/// config actually comes back on.
fn apply_hook_overrides(interp: &mut tcl::Interpreter, disabled: &[String]) {
    if interp.eval("::veles::hooks_off_clear").is_err() {
        return; // bootstrap did not run (no hooks machinery to configure)
    }
    for entry in disabled {
        let Some((hook, handler)) = crate::config::HooksSection::split_entry(entry) else {
            tracing::warn!("[hooks] disabled: `{entry}` is not `<hook> <handler>` — ignored");
            continue;
        };
        let script = format!("::veles::hook_disable {} {}", quote(hook), quote(handler));
        if let Err(e) = interp.eval(script.as_str()) {
            tracing::warn!("could not disable hook `{entry}`: {e}");
        }
    }
}

/// Load plugins:
///  1. manifest-based: <home>/plugins/<name>/plugin.toml + entry
///     (honors config plugins.enabled/disabled, hermes semantics).
///     `kind = "tcl"` is sourced here; `kind = "rust"` is dlopened via
///     rustplugins::load_rust_manifests (Phase B) — both kinds end up
///     in the same hook/tool/filter machinery.
///  2. VELES_PLUGIN_DIRS (comma-separated dirs, dev override; each dir
///     may contain plugin.tcl or plugin.so)
fn load_plugins(interp: &mut tcl::Interpreter) -> Result<()> {
    let cfg = crate::config::load().unwrap_or_default();
    let discovered = crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled)?;
    // Re-apply `[hooks] disabled` on EVERY load, including the watcher's.
    // Sourcing a plugin re-registers its handlers, so without this a
    // handler the user switched off would come back the next time
    // anyone saved a plugin file.
    apply_hook_overrides(interp, &cfg.hooks.disabled);
    crate::rustplugins::load_rust_manifests(&discovered);
    for p in &discovered {
        if p.manifest.kind != "tcl" {
            continue; // kind = "rust" is handled by load_rust_manifests
        }
        if is_unloaded(&p.manifest.name) {
            tracing::info!("plugin {}: unloaded; not sourced", p.manifest.name);
            continue;
        }
        source_plugin(interp, p)?;
    }
    if let Ok(dirs) = std::env::var("VELES_PLUGIN_DIRS") {
        for d in dirs.split(',') {
            let d = d.trim();
            if d.is_empty() {
                continue;
            }
            let dir = std::path::Path::new(d);
            let entry = dir.join("plugin.tcl");
            if entry.exists() {
                interp
                    .source(&entry)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", entry.display()))?;
                tracing::info!("loaded dev plugin: {}", entry.display());
                continue;
            }
            // Dev-override parity for Phase B: a dir with plugin.so (and
            // no plugin.tcl) loads through the same ABI as manifests do.
            let so = dir.join("plugin.so");
            if so.exists() {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "dev-so-plugin".to_string());
                if let Err(e) = crate::rustplugins::load_so(&so, &name, "so") {
                    tracing::error!("dev .so plugin {} failed to load: {e:#}", so.display());
                } else {
                    tracing::info!("loaded dev .so plugin: {}", so.display());
                }
            }
        }
    }
    Ok(())
}

/// Source one Tcl plugin under its owner tag (R7): the thread is
/// tagged `tcl:<name>` for the duration of the `source`, so every
/// `veles::register_*` the script makes while being sourced is
/// attributed to it — the same rule a module's instance thread
/// follows — and `::veles::plugin_current` tells the Tcl side the same
/// thing for its hook registry. A handler registered later, from a
/// callback, is core's: the tag is scoped to the sourcing.
fn source_plugin(interp: &mut tcl::Interpreter, p: &crate::plugins::LoadedPlugin) -> Result<()> {
    let entry = p.dir.join(&p.manifest.entry);
    if !entry.exists() {
        tracing::warn!(
            "plugin {} missing entry {}",
            p.manifest.name,
            entry.display()
        );
        return Ok(());
    }
    let tag = owner_tag_for(&p.manifest.name);
    crate::plugins::declare_services(p);
    interp.set("::veles::plugin_current", p.manifest.name.as_str());
    let sourced = crate::lifecycle::with_thread_owner(tag, || interp.source(&entry));
    interp.set("::veles::plugin_current", "");
    sourced.map_err(|e| anyhow::anyhow!("{}: {e}", entry.display()))?;
    tracing::info!("loaded plugin: {}", p.manifest.name);
    Ok(())
}

/// Drop everything plugin `name` registered — its hook handlers on the
/// Tcl side, its tools/commands/providers on core's by owner — and
/// keep it out of every later load until `Load` names it. Its procs
/// stay defined in the interpreter (harmless, and a re-load redefines
/// them); nothing calls them any more.
fn unload_plugin(interp: &mut tcl::Interpreter, name: &str) -> Result<String, String> {
    let dropped = interp
        .eval(format!("::veles::plugin_unload {}", quote(name)).as_str())
        .map(|v| v.to_string())
        .map_err(|e| e.to_string())?;
    publish_hook_registry(interp);
    let purged = crate::modulehost::purge_owner_everywhere(Some(owner_tag_for(name)));
    unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(name.to_string());
    tracing::info!("plugin {name}: unloaded ({dropped} hook handler(s), {purged} registration(s))");
    Ok(format!(
        "{name}: unloaded ({dropped} hook handler(s) dropped, {purged} registration(s) purged); \
         `start` loads it again"
    ))
}

/// Source plugin `name` again, what it had purged first (a loaded
/// plugin's reload must not double-register), and off the unloaded
/// list.
fn load_one(interp: &mut tcl::Interpreter, name: &str) -> Result<String, String> {
    let cfg = crate::config::load().unwrap_or_default();
    let discovered = crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled)
        .map_err(|e| e.to_string())?;
    let Some(p) = discovered
        .iter()
        .find(|p| p.manifest.name == name && p.manifest.kind == "tcl")
    else {
        return Err(format!(
            "no enabled Tcl plugin named `{name}` (`veles plugins list` names them)"
        ));
    };
    let dropped = interp
        .eval(format!("::veles::plugin_unload {}", quote(name)).as_str())
        .map(|v| v.to_string())
        .unwrap_or_default();
    let purged = crate::modulehost::purge_owner_everywhere(Some(owner_tag_for(name)));
    unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(name);
    source_plugin(interp, p).map_err(|e| e.to_string())?;
    publish_hook_registry(interp);
    Ok(format!(
        "{name}: loaded ({dropped} hook handler(s) and {purged} registration(s) of its earlier \
         load purged first)"
    ))
}

/// Core bridge commands exposed to Tcl. This is the "bidirectional" part:
/// plugin code gets real engine capabilities, not just string eval.
///
/// NOTE: the oooutlk/tcltk crate exposes commands to Tcl by evaluating
/// `proc` definitions (slopdrop's pattern), not via a Rust create_command
/// API. Procs that need to call back into Rust go through a single
/// `veles::ffi` channel command that the Rust side replaces at runtime
/// once the crate's command-registration story is confirmed; until then
/// bridge data is synced via globals before each eval (slopdrop
/// sync_channel_members pattern) and results come back as return values.
fn register_bridge_commands(interp: &mut tcl::Interpreter, home: PathBuf) {
    // Static engine facts, synced as globals. `home` is the CONFIG root
    // (see spawn); `::veles_home` keeps its historical DATA-root meaning.
    // Do not reconstruct a named profile's roots from VELES_HOME.
    let boot = format!(
        "set ::veles_version {}; set ::veles_home {}; \
         set ::veles_config_dir {}; set ::veles_data_dir {}; \
         set ::veles_cache_dir {}; set ::veles_profile {}",
        quote(crate::VERSION),
        quote(&crate::config::data_dir().to_string_lossy()),
        quote(&home.to_string_lossy()),
        quote(&crate::config::data_dir().to_string_lossy()),
        quote(&crate::config::cache_dir().to_string_lossy()),
        quote(&crate::profiles::active_profile()),
    );
    let _ = interp.eval(boot.as_str());

    // Hook dispatch convention: plugins register via
    //   veles::hook <name> <procname>
    // and the core calls veles::dispatch_hook <name> <args-json>.
    let _ = interp.eval(
        r#"
        namespace eval ::veles {}
        array set ::veles::hooks {}
        # Which plugin each handler came from (R7): the loader sets
        # ::veles::plugin_current around each plugin's `source`, so a
        # handler registered while a plugin is sourced is that plugin's
        # and leaves with it (`::veles::plugin_unload`).
        array set ::veles::hook_owner {}
        if {![info exists ::veles::plugin_current]} { set ::veles::plugin_current "" }
        # Handlers switched OFF. Deliberately a separate array from
        # ::veles::hooks: a reload wipes the registry and re-sources every
        # plugin, and a switch that silently came back on after a file
        # save would be worse than not having one.
        if {![info exists ::veles::hooks_off]} { array set ::veles::hooks_off {} }

        # Register a proc as a handler for a lifecycle hook.
        proc ::veles::hook {hookname procname} {
            lappend ::veles::hooks($hookname) $procname
            set ::veles::hook_owner([::veles::_off_key $hookname $procname]) $::veles::plugin_current
        }

        # Drop every handler plugin `name` registered; returns how many.
        proc ::veles::plugin_unload {name} {
            set n 0
            foreach k [array names ::veles::hooks] {
                set keep {}
                foreach h $::veles::hooks($k) {
                    set key [::veles::_off_key $k $h]
                    set owner ""
                    if {[info exists ::veles::hook_owner($key)]} { set owner $::veles::hook_owner($key) }
                    if {$owner eq $name} {
                        incr n
                        unset -nocomplain ::veles::hook_owner($key)
                    } else {
                        lappend keep $h
                    }
                }
                if {[llength $keep]} { set ::veles::hooks($k) $keep } else { unset ::veles::hooks($k) }
            }
            return $n
        }

        # One spelling of the off-list key, so the disable, the enable
        # and the dispatch check cannot disagree about it. \x1f cannot
        # appear in a hook name or a proc name.
        proc ::veles::_off_key {hookname procname} {
            return "$hookname\x1f$procname"
        }

        proc ::veles::hook_disable {hookname procname} {
            set ::veles::hooks_off([::veles::_off_key $hookname $procname]) 1
            return "off: $procname on $hookname"
        }

        proc ::veles::hook_enable {hookname procname} {
            unset -nocomplain ::veles::hooks_off([::veles::_off_key $hookname $procname])
            return "on: $procname on $hookname"
        }

        proc ::veles::hook_is_off {hookname procname} {
            return [info exists ::veles::hooks_off([::veles::_off_key $hookname $procname])]
        }

        # Drop every override — the core re-applies the persisted set
        # after each load, so it owns the list and this is the reset.
        proc ::veles::hooks_off_clear {} {
            array unset ::veles::hooks_off
            array set ::veles::hooks_off {}
            return ""
        }

        # Called by the Rust core: hook args arrive in ::veles::hook_args
        # (set by the core via interp.set — JSON braces survive intact).
        # Runs every registered handler in registration order; first
        # non-empty return wins (hermes' transform-hook semantics).
        # A switched-off handler is skipped, not removed: the row has to
        # stay visible in the manager to be switched back on.
        proc ::veles::dispatch_hook {hookname} {
            if {![info exists ::veles::hooks($hookname)]} { return "" }
            foreach h $::veles::hooks($hookname) {
                if {[::veles::hook_is_off $hookname $h]} { continue }
                set r [$h]
                if {$r ne ""} { return $r }
            }
            return ""
        }

        # List registered hooks (introspection for the CLI and the Dev
        # pane inspector).
        proc ::veles::hooks_list {} {
            set out {}
            foreach k [lsort [array names ::veles::hooks]] {
                set row {}
                foreach h $::veles::hooks($k) {
                    lappend row [expr {[::veles::hook_is_off $k $h] ? "$h (off)" : $h}]
                }
                lappend out "$k: [join $row { }]"
            }
            return [join $out \n]
        }

        proc ::veles::_json_escape {s} {
            return [string map [list \\ \\\\ \" \\\" \n \\n \r \\r \t \\t] $s]
        }

        # The hooks manager's payload: every hook, its handlers in
        # dispatch order, and whether each one will actually run.
        proc ::veles::hooks_json {} {
            set out {}
            foreach k [lsort [array names ::veles::hooks]] {
                set handlers {}
                foreach h $::veles::hooks($k) {
                    set en [expr {[::veles::hook_is_off $k $h] ? "false" : "true"}]
                    lappend handlers "{\"proc\":\"[::veles::_json_escape $h]\",\"enabled\":$en}"
                }
                lappend out "{\"name\":\"[::veles::_json_escape $k]\",\"handlers\":\[[join $handlers ,]\]}"
            }
            return "\[[join $out ,]\]"
        }
        "#,
    );
}

/// Git-backed plugin state (slopdrop pattern): init a repo at
/// <home>/plugins/.git on first use and commit any changes so every
/// live edit is versioned and rollbackable (`veles plugins rollback`).
/// Runs the system git binary (flake wraps it onto PATH).
fn git_snapshot_plugins(home: &Path) {
    let dir = home.join("plugins");
    if !dir.exists() {
        return;
    }
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("GIT_AUTHOR_NAME", "veles")
            .env("GIT_AUTHOR_EMAIL", "veles@local")
            .env("GIT_COMMITTER_NAME", "veles")
            .env("GIT_COMMITTER_EMAIL", "veles@local")
            .output()
    };
    if !dir.join(".git").exists() {
        if let Err(e) = run(&["init", "-q"]) {
            tracing::warn!("plugins git init failed: {e}");
            return;
        }
    }
    let _ = run(&["add", "-A"]);
    // Only commit when something actually changed.
    let status = run(&["status", "--porcelain"]).ok();
    let dirty = status
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    if dirty {
        let _ = run(&["commit", "-q", "-m", "veles plugin state snapshot"]);
        tracing::info!("plugin state committed");
    }
}

/// How long to coalesce a burst of filesystem events before acting.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Does this event mean a human changed a plugin, or is it just us
/// reading the files?
///
/// This distinction is the whole ballgame. `notify`'s inotify backend
/// watches `IN_OPEN` and reports `CLOSE_NOWRITE`, and reloading opens
/// every plugin.toml/plugin.tcl to re-source it. The original filter
/// looked only at the file EXTENSION, so a reload's own reads looked
/// exactly like an edit and scheduled another reload — for ever. One
/// gateway process logged 72,914 reloads in a single lifetime (~2/sec,
/// 145k journal lines a day) with the plugin files untouched on disk.
fn is_reload_trigger(ev: &notify::Event) -> bool {
    // Reads are never a change. Access covers Open, Close(Read), Read.
    if matches!(ev.kind, notify::EventKind::Access(_)) {
        return false;
    }
    ev.paths.iter().any(|p| is_plugin_source(p))
}

/// A file whose change means a plugin changed: a manifest, a Tcl
/// source, or a script-tier source (`scripthost::SCRIPT_SOURCE_EXTS`).
/// The event filter and the change digest both go through here, so the
/// two can never disagree about what a plugin source is.
fn is_plugin_source(p: &Path) -> bool {
    match p.extension().and_then(|x| x.to_str()) {
        Some("tcl") | Some("toml") => true,
        Some(ext) => crate::scripthost::SCRIPT_SOURCE_EXTS.contains(&ext),
        None => false,
    }
}

/// Content hash of every plugin source under `dir`, or `None` if the
/// tree can't be read. Path names are hashed alongside the bytes so a
/// rename is a change even when the content is identical.
fn plugin_digest(dir: &Path) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut files = Vec::new();
    collect_plugin_sources(dir, &mut files);
    files.sort();
    let mut h = Sha256::new();
    for f in &files {
        h.update(f.to_string_lossy().as_bytes());
        h.update(std::fs::read(f).ok()?);
    }
    Some(h.finalize().into())
}

fn collect_plugin_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            // Skip the git-backed state dir: snapshots churn constantly
            // and none of it is a plugin source.
            if p.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            collect_plugin_sources(&p, out);
        } else if is_plugin_source(&p) {
            out.push(p);
        }
    }
}

/// The tooling declaration stub, compiled in from the repo so there is
/// no install-layout or resource-path question anywhere.
const VELES_API_STUB: &str = include_str!("../../../plugins/veles_api.tcl");

/// Copy [`VELES_API_STUB`] into `<home>/plugins/` so a language server
/// rooted there resolves the plugin API.
///
/// `veles::call_llm`, `veles::json_get` and `veles::routes` are native
/// commands (Rust `Tcl_CreateObjCommand`) and the hook registry is an
/// eval'd prelude string — none of them exists in a file any static
/// index can read. tcl-lsp could silence the lint noise for them (the
/// host command db knows the names) but had nothing to hover or jump to,
/// so plugin code showed documentation for `set` and `puts` and nothing
/// for the API the plugin is actually written against. The stub is
/// declarations only; it is never sourced (no manifest, and `load_plugins`
/// skips plain files), and writing it before the watcher starts keeps it
/// out of the reload loop.
fn sync_api_stub(home: &Path) {
    let dir = home.join("plugins");
    if !dir.exists() {
        return; // nothing lives there; a failed mkdir is not boot's problem
    }
    let path = dir.join("veles_api.tcl");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == VELES_API_STUB {
            return; // unchanged: no write, no event, no plugin snapshot churn
        }
    }
    match std::fs::write(&path, VELES_API_STUB) {
        Ok(()) => tracing::info!("plugin API stub refreshed: {}", path.display()),
        Err(e) => tracing::warn!("could not write plugin API stub: {e}"),
    }
}

/// Hot reload: watch <home>/plugins for .tcl/.toml changes and send a
/// Reload request into the Tcl thread. Debounced 500ms by coalescing.
fn start_plugin_watcher(home: PathBuf, tx: mpsc::Sender<TclRequest>) {
    let dir = home.join("plugins");
    if !dir.exists() {
        return;
    }
    std::thread::Builder::new()
        .name("veles-plugin-watch".into())
        .spawn(move || {
            use notify::{RecursiveMode, Watcher};
            let (wtx, wrx) = std::sync::mpsc::channel();
            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = wtx.send(res);
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("plugin watcher init failed: {e}");
                    return;
                }
            };
            if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
                tracing::warn!("plugin watcher watch failed: {e}");
                return;
            }
            let mut last_digest = plugin_digest(&dir);
            loop {
                match wrx.recv() {
                    Ok(Ok(ev)) => {
                        if !is_reload_trigger(&ev) {
                            continue;
                        }
                        // Coalesce a burst: editors write, rename and
                        // chmod the same file in quick succession.
                        std::thread::sleep(DEBOUNCE);
                        while wrx.try_recv().is_ok() {}

                        // Second guard: reload only if the plugin sources
                        // actually differ. Catches `touch`, mtime-only
                        // churn and save-with-no-edit.
                        let digest = plugin_digest(&dir);
                        if digest == last_digest {
                            continue;
                        }

                        tracing::info!("plugins changed — reloading");
                        let (reply, done) = oneshot::channel();
                        if tx.send(TclRequest::Reload { reply }).is_err() {
                            return; // Tcl thread gone
                        }
                        // Wait for the reload to FINISH, then drain. The
                        // reload re-reads every plugin file and those
                        // reads raise events; draining before dispatch
                        // (as this used to) always left them queued, so
                        // the next recv() fired immediately and the
                        // watcher retriggered itself forever.
                        let _ = done.blocking_recv();
                        // The script tier (R9) re-loads its hosted kinds
                        // on the same trigger; its loads read the same
                        // files, so they too happen before the drain.
                        for line in crate::scripthost::reload_all_blocking() {
                            tracing::info!("plugins changed: {line}");
                        }
                        while wrx.try_recv().is_ok() {}
                        last_digest = digest;
                    }
                    Ok(Err(e)) => tracing::warn!("plugin watch error: {e}"),
                    Err(_) => return, // watcher dropped
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Lay out <home>/plugins/demo/{plugin.tcl,plugin.toml}.
    fn scratch_plugins() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().expect("tempdir");
        let demo = home.path().join("plugins").join("demo");
        std::fs::create_dir_all(&demo).expect("mkdir");
        std::fs::write(demo.join("plugin.tcl"), "# demo plugin\n").expect("write tcl");
        std::fs::write(demo.join("plugin.toml"), "name = \"demo\"\n").expect("write toml");
        let home_path = home.path().to_path_buf();
        (home, home_path)
    }

    /// THE regression test for the self-sustaining reload loop.
    ///
    /// `load_plugins` opens every plugin.toml/plugin.tcl to re-source it.
    /// notify's inotify backend watches IN_OPEN, so those reads produce
    /// events on paths ending in .tcl/.toml. The old extension-only
    /// filter accepted them and scheduled another reload, for ever: one
    /// gateway process logged 72,914 reloads in a single lifetime.
    ///
    /// Reading the plugin files must dispatch NOTHING.
    #[test]
    fn reading_plugin_files_never_triggers_a_reload() {
        let (_guard, home) = scratch_plugins();
        let demo = home.join("plugins").join("demo");
        let (tx, rx) = mpsc::channel();
        start_plugin_watcher(home.clone(), tx);
        // Let the inotify watch be established before touching anything.
        std::thread::sleep(Duration::from_millis(300));

        // Exactly what a reload does: read every plugin source, repeatedly.
        for _ in 0..5 {
            let _ = std::fs::read(demo.join("plugin.toml")).expect("read toml");
            let _ = std::fs::read(demo.join("plugin.tcl")).expect("read tcl");
            std::thread::sleep(Duration::from_millis(20));
        }

        // Generously longer than DEBOUNCE, so a loop would have fired.
        let got = rx.recv_timeout(DEBOUNCE + Duration::from_millis(1200));
        assert!(
            got.is_err(),
            "reading the plugin files scheduled a reload — the watcher is \
             retriggering itself again"
        );
    }

    /// The other half: the guards above must not have killed hot reload.
    /// A watcher that never reloads would pass the test above trivially.
    #[test]
    fn editing_a_plugin_does_trigger_a_reload() {
        let (_guard, home) = scratch_plugins();
        let demo = home.join("plugins").join("demo");
        let (tx, rx) = mpsc::channel();
        start_plugin_watcher(home.clone(), tx);
        std::thread::sleep(Duration::from_millis(300));

        std::fs::write(demo.join("plugin.tcl"), "# edited by the test\n").expect("edit");

        let got = rx.recv_timeout(DEBOUNCE + Duration::from_secs(4));
        assert!(
            matches!(got, Ok(TclRequest::Reload { .. })),
            "a real edit must still hot-reload, got {got:?}"
        );
    }

    /// The script tier (R9): an edit to a `.ts` is a change exactly as
    /// an edit to a `.tcl` is. Found on the scratch gateway on
    /// 2026-10-10: the event filter and the change digest both knew only
    /// tcl/toml, so a script edit raised an event the watcher dropped,
    /// and the digest would not have moved either.
    #[test]
    fn editing_a_script_plugin_does_trigger_a_reload() {
        let (_guard, home) = scratch_plugins();
        let js = home.join("plugins").join("demo-js");
        std::fs::create_dir_all(&js).expect("mkdir");
        std::fs::write(
            js.join("plugin.toml"),
            "name = \"demo-js\"\nkind = \"js\"\nentry = \"plugin.ts\"\n",
        )
        .expect("write toml");
        std::fs::write(js.join("plugin.ts"), "const n: number = 7;\n").expect("write ts");
        let (tx, rx) = mpsc::channel();
        start_plugin_watcher(home.clone(), tx);
        std::thread::sleep(Duration::from_millis(300));

        std::fs::write(js.join("plugin.ts"), "const n: number = 8;\n").expect("edit");

        let got = rx.recv_timeout(DEBOUNCE + Duration::from_secs(4));
        assert!(
            matches!(got, Ok(TclRequest::Reload { .. })),
            "a script edit must hot-reload like a Tcl edit, got {got:?}"
        );
    }

    /// The digest half on its own: a script source's bytes are part of
    /// the change digest, so a `.ts` edit moves it and a rewrite with
    /// the same bytes does not. (The event filter can let a script edit
    /// through and the digest guard still swallow it.)
    #[test]
    fn the_change_digest_covers_the_script_tier_sources() {
        let (_guard, home) = scratch_plugins();
        let dir = home.join("plugins");
        // Every extension a hosted kind is written in (js, ts, rkt, …):
        // a new host that forgets to list its own fails here.
        for ext in crate::scripthost::SCRIPT_SOURCE_EXTS {
            let d = dir.join(format!("demo-{ext}"));
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(
                d.join("plugin.toml"),
                format!("name = \"demo-{ext}\"\nentry = \"plugin.{ext}\"\n"),
            )
            .expect("write toml");
            let entry = d.join(format!("plugin.{ext}"));
            std::fs::write(&entry, "; seven\n").expect("write entry");
            let before = plugin_digest(&dir).expect("digest");
            std::fs::write(&entry, "; seven\n").expect("rewrite");
            assert_eq!(
                plugin_digest(&dir).expect("digest"),
                before,
                ".{ext}: the same bytes are the same digest"
            );
            std::fs::write(&entry, "; eight\n").expect("edit");
            assert_ne!(
                plugin_digest(&dir).expect("digest"),
                before,
                "a .{ext} edit must move the digest"
            );
        }
        assert!(
            crate::scripthost::SCRIPT_SOURCE_EXTS.contains(&"rkt"),
            "the Racket host's sources are watched"
        );
        assert!(
            crate::scripthost::SCRIPT_SOURCE_EXTS.contains(&"lisp"),
            "the Lisp host's sources are watched"
        );
    }

    /// Rewriting a file with identical bytes is not a change.
    #[test]
    fn rewriting_identical_content_does_not_reload() {
        let (_guard, home) = scratch_plugins();
        let demo = home.join("plugins").join("demo");
        let (tx, rx) = mpsc::channel();
        start_plugin_watcher(home.clone(), tx);
        std::thread::sleep(Duration::from_millis(300));

        // Same bytes back — mtime moves, content does not.
        std::fs::write(demo.join("plugin.tcl"), "# demo plugin\n").expect("rewrite");

        let got = rx.recv_timeout(DEBOUNCE + Duration::from_millis(1200));
        assert!(
            got.is_err(),
            "identical content must not reload, got {got:?}"
        );
    }

    /// The stub must not drift from reality: every native command the
    /// bridge registers must be declared, and every prelude proc that
    /// `register_bridge_commands` evals into a live interpreter must be
    /// declared too. This is what makes the tooling answer trustworthy:
    /// if the core gains `veles::foo` and the stub misses it, this test
    /// fails — the plugin author's hover silently missing it would not.
    #[test]
    fn the_api_stub_declares_every_veles_command() {
        // Names the bridge registers natively (Tcl_CreateObjCommand).
        // Scanned over whitespace-stripped source because the call wraps
        // across lines for json_get — a line-by-line regex would miss it.
        let bridge_src = include_str!("bridge.rs");
        let joined: String = bridge_src.chars().filter(|c| !c.is_whitespace()).collect();
        let marker = "def_proc_with_client_data(\"veles::";
        let mut natives: Vec<String> = Vec::new();
        let mut cursor = 0;
        while let Some(rel) = joined[cursor..].find(marker) {
            let start = cursor + rel + marker.len();
            let end = joined[start..]
                .find('\"')
                .map(|e| start + e)
                .unwrap_or(joined.len());
            natives.push(format!("veles::{}", &joined[start..end]));
            cursor = end;
        }
        assert!(
            natives.len() >= 3,
            "bridge must register call_llm/json_get/routes, found {natives:?}"
        );

        // THE THIRD LEG, added 2026-09-04 with the op extraction.
        //
        // The engine capabilities now live in `bridgeops::OP_NAMES`, a
        // language-neutral table that BOTH plugin surfaces adapt over:
        // Tcl through the `op_command!` shims scraped above, and
        // out-of-tree Rust through the ABI's `host_call`. Parity used
        // to be a claim in a docstring; this is what makes it a
        // property. If somebody adds an op and wires only Tcl to it,
        // the `.so` half silently lacks it — so the two lists must be
        // equal, not merely overlapping.
        //
        // `TCL_ONLY_COMMANDS` are the registrars whose last argument is
        // a PROC NAME (`register_tool`, `register_command`). They have
        // no table entry because the C side takes a function pointer
        // instead: same capability, different shape, so they are named
        // as the exception rather than quietly tolerated.
        let expected_natives: std::collections::BTreeSet<String> = crate::bridgeops::OP_NAMES
            .iter()
            .chain(crate::bridgeops::TCL_ONLY_COMMANDS.iter())
            .map(|n| format!("veles::{n}"))
            .collect();
        let got_natives: std::collections::BTreeSet<String> = natives.iter().cloned().collect();
        assert_eq!(
            got_natives, expected_natives,
            "bridge.rs registers a different set than bridgeops declares — \
             every neutral op needs a Tcl command, and every Tcl command that \
             is not in TCL_ONLY_COMMANDS needs a table entry so the .so ABI \
             reaches it too"
        );

        let stub = VELES_API_STUB.to_string();
        for name in &natives {
            let decl = format!("proc ::{name} ");
            assert!(
                stub.contains(&decl),
                "native command {name} missing from veles_api.tcl"
            );
        }

        // Prelude procs: ask a real interpreter what it has.
        let mut interp = tcl::Interpreter::new().expect("tcl interp");
        register_bridge_commands(&mut interp, PathBuf::from("/tmp"));
        let out = interp
            .eval("lsort [info commands ::veles::*]")
            .expect("info commands")
            .to_string();
        for qname in out.split_whitespace() {
            // ::veles::_off_key and friends: private helpers are declared
            // too where plugins see them called, but underscore names are
            // exempt — plugins must not call them anyway.
            let leaf = qname.rsplit(':').next().unwrap_or(qname);
            if leaf.starts_with('_') {
                continue;
            }
            let bare = qname.trim_start_matches(':');
            let decl = format!("proc ::{bare} ");
            assert!(
                stub.contains(&decl),
                "prelude command {qname} missing from veles_api.tcl"
            );
        }

        // THE OTHER DIRECTION, added 2026-09-04. Everything above proves
        // the stub is not missing anything; nothing proved the stub does
        // not INVENT anything.
        //
        // That failure mode is the worse one. `sync_api_stub` copies
        // this file into the user's plugins directory as the API
        // reference, so a `proc ::veles::whatever` with no
        // implementation behind it is a documented command that a
        // plugin author writes code against and discovers at run time,
        // as "invalid command name".
        //
        // (The same one-directional hole was found the same day in the
        // slash-command taxonomy — frontends/commands.rs F8 — where it
        // had let 31 commands diverge unpoliced. It is worth checking
        // for wherever a taxonomy test exists.)
        // BOTH halves of the surface: `out` is what the prelude defines
        // in a live interpreter, `natives` is what bridge.rs registers
        // through Tcl_CreateObjCommand. Neither alone is the API — the
        // test's own forward direction already checks them separately,
        // and using only one here reported `call_llm` (a native) as
        // unimplemented.
        let live: std::collections::BTreeSet<String> = out
            .split_whitespace()
            .map(|q| q.trim_start_matches(':').to_string())
            .chain(natives.iter().cloned())
            .collect();
        for line in stub.lines() {
            let Some(rest) = line.strip_prefix("proc ::veles::") else {
                continue;
            };
            let name = rest
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap_or("");
            if name.is_empty() || name.starts_with('_') {
                continue;
            }
            assert!(
                live.contains(&format!("veles::{name}")),
                "veles_api.tcl declares ::veles::{name}, and nothing implements it — \
                 a plugin author reading the stub would call a command that does not exist"
            );
        }
    }

    #[test]
    fn sync_api_stub_lands_and_is_idempotent() {
        let (_home, home_path) = scratch_plugins();
        let path = home_path.join("plugins").join("veles_api.tcl");
        assert!(!path.exists(), "scratch layout has no stub yet");
        sync_api_stub(&home_path);
        assert_eq!(
            std::fs::read_to_string(&path).expect("stub written"),
            VELES_API_STUB
        );
        // second call must not rewrite (mtime unchanged ⇒ no watcher event
        // ⇒ no git snapshot churn)
        let before = std::fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime");
        std::thread::sleep(Duration::from_millis(30));
        sync_api_stub(&home_path);
        let after = std::fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert_eq!(before, after, "unchanged stub must not be rewritten");
    }

    #[test]
    fn access_events_are_never_reload_triggers() {
        use notify::event::{AccessKind, AccessMode};
        let tcl = PathBuf::from("/x/plugins/demo/plugin.tcl");
        let read_open = notify::Event {
            kind: notify::EventKind::Access(AccessKind::Open(AccessMode::Read)),
            paths: vec![tcl.clone()],
            attrs: Default::default(),
        };
        assert!(!is_reload_trigger(&read_open));

        let modified = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![tcl],
            attrs: Default::default(),
        };
        assert!(is_reload_trigger(&modified));

        // Non-plugin files are ignored whatever the kind.
        let other = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from("/x/plugins/demo/README.md")],
            attrs: Default::default(),
        };
        assert!(!is_reload_trigger(&other));
    }

    /// The blocking eval must refuse inside a runtime rather than panic
    /// with "Cannot block the current thread from within a runtime" —
    /// that panic is what poisoned the store on 2026-08-22.
    #[test]
    fn blocking_eval_refuses_inside_a_runtime() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let (tx, _rx) = mpsc::channel::<TclRequest>();
        let handle = TclHandle { tx };
        let err = rt
            .block_on(async { handle.eval("set x 1") })
            .expect_err("must refuse, not block");
        assert!(err.contains("async context"), "got: {err}");
    }

    /// Env var restored on drop even when an assertion fails mid-test —
    /// VELES_PLUGIN_DIRS is process-global and every other tclrt spawn
    /// in this binary reads it.
    struct EnvCleaner(&'static str);
    impl Drop for EnvCleaner {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    /// The three sample plugins (plugins/examples/) are the plugin
    /// system's reference surface — load them through the REAL boot
    /// path (spawn → load_plugins → VELES_PLUGIN_DIRS) and exercise one
    /// capability each: tool registration + call, gateway-filter
    /// verdict, store reads + plugin note. If a sample breaks, plugin
    /// authors copy a broken template — so the gate owns them.
    #[test]
    fn the_sample_plugins_load_and_work() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");
        let examples = format!("{}/../../plugins/examples", env!("CARGO_MANIFEST_DIR"));
        std::env::set_var(
            "VELES_PLUGIN_DIRS",
            format!(
                "{examples}/tcl-wordcount,{examples}/tcl-alias-filter,{examples}/tcl-session-digest"
            ),
        );
        let _clean = EnvCleaner("VELES_PLUGIN_DIRS");
        let handle = spawn(home.path().to_path_buf()).expect("spawn");

        // 1. wordcount — registered through veles::register_tool; a
        //    registry built AFTER the load picks it up (extras flow),
        //    and tool_call reaches it via the Tcl fast path.
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("state.db")).expect("store"),
        ));
        let reg = crate::tools::builtin_registry(store.clone(), None, "__sample_plugins__");
        assert!(
            reg.schema_for("wordcount").is_some(),
            "wordcount schema must be visible to a fresh registry"
        );
        crate::tools::set_global_registry(std::sync::Arc::new(reg));
        let out = handle
            .eval(r#"veles::tool_call wordcount {"text": "one two three"}"#)
            .expect("tool_call");
        assert!(out.contains("words=3"), "{out}");

        // 2. alias-filter — pre_gateway_dispatch registered and its
        //    rewrite verdict carries the canned answer.
        let hooks = handle.eval("::veles::hooks_list").expect("hooks_list");
        assert!(hooks.contains("pre_gateway_dispatch"), "{hooks}");
        let verdict = handle
            .eval("set ::veles::hook_args {{\"text\":\"!ping\"}}; alias_filter::handle")
            .expect("filter handle");
        assert!(
            verdict.contains("rewrite") && verdict.contains("pong"),
            "{verdict}"
        );
        // Anything but the alias passes through untouched.
        let verdict = handle
            .eval("set ::veles::hook_args {{\"text\":\"hello\"}}; alias_filter::handle")
            .expect("filter passthrough");
        assert_eq!(verdict, "");

        // 3. session-digest — reads sessions/history from the store and
        //    parks a plugin-action note; the turn stays untouched.
        {
            let s = store.lock().unwrap();
            s.create_session("digest_sample").expect("session");
            for i in 0..4 {
                s.append_message(
                    "digest_sample",
                    &crate::providers::ChatMessage::user(&format!("message number {i}")),
                )
                .expect("message");
            }
        }
        let _ = crate::notes::take_plugin_notes(); // drain: process-global lot
        let verdict = handle
            .eval("set ::veles::hook_args {{\"text\":\"hi\"}}; session_digest::handle")
            .expect("digest handle");
        assert_eq!(verdict, "", "informational hooks must not touch the turn");
        let notes = crate::notes::take_plugin_notes();
        assert!(
            notes.iter().any(|n| n.summary.contains("digest_sample")),
            "the digest must park a note: {notes:?}"
        );
    }

    /// The bundled tier through the REAL boot path (0.1.2): a fresh
    /// config dir with no plugins of its own, and the Anthropic driver
    /// in a bundled root — laid out the way the release artifacts lay
    /// it out — registers `anthropic-messages` at spawn. And the bundled
    /// plugin is not copied into, or committed to, the user's plugins
    /// repo: the bundled root is read-only to veles.
    #[test]
    fn a_bundled_tcl_plugin_loads_at_boot_and_stays_out_of_the_users_repo() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");
        let shipped = tempfile::tempdir().expect("bundled root");
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/examples/tcl-anthropic-driver");
        let dst = shipped.path().join("tcl-anthropic-driver");
        std::fs::create_dir_all(&dst).expect("bundled dir");
        for f in ["plugin.toml", "plugin.tcl"] {
            std::fs::copy(src.join(f), dst.join(f)).expect("copy the driver");
        }
        std::env::set_var(crate::plugins::BUNDLED_ENV, shipped.path());
        crate::providers::purge_owner_drivers(Some(owner_tag_for("tcl-anthropic-driver")));

        let handle = spawn(home.path().to_path_buf()).expect("spawn");
        handle.wait_ready().expect("ready");
        assert!(
            crate::providers::driver("anthropic-messages").is_some(),
            "the bundled driver must register at boot; registered: {:?}",
            crate::providers::drivers()
                .iter()
                .map(|d| d.name.clone())
                .collect::<Vec<_>>()
        );
        let user = crate::plugins::plugins_dir();
        assert!(!user.join("tcl-anthropic-driver").exists());
        let tracked = std::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(&user)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        assert!(!tracked.contains("anthropic"), "{tracked}");
        drop(handle);
        crate::providers::purge_owner_drivers(Some(owner_tag_for("tcl-anthropic-driver")));
    }

    /// Per-plugin ownership and unload (R7). A plugin sourced by the
    /// loader registers a tool, a command and a hook handler; `Unload`
    /// removes all three — the tool and the command from core's
    /// registries by owner `tcl:<name>`, the handler from the
    /// interpreter's — and a full `Reload` (the watcher's path) leaves
    /// it out until `Load` names it, which brings all three back.
    #[test]
    fn unloading_a_tcl_plugin_drops_its_registrations_and_load_brings_them_back() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");
        let dir = home.path().join("plugins").join("ownprobe");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("plugin.toml"), "name = \"ownprobe\"\n").expect("toml");
        std::fs::write(
            dir.join("plugin.tcl"),
            r#"
namespace eval ownprobe {}
proc ownprobe::tool {} { return "probe" }
proc ownprobe::cmd {} { return "probe prompt" }
proc ownprobe::hook {} { return "" }
::veles::register_tool ownprobe_tool "an owned tool" {{"type":"object"}} ownprobe::tool
::veles::register_command ownprobe_cmd "an owned command" ownprobe::cmd
::veles::hook on_session_reset ownprobe::hook
"#,
        )
        .expect("tcl");
        let handle = spawn(home.path().to_path_buf()).expect("spawn");
        // The load runs on the Tcl thread; an eval is the barrier that
        // orders the registry reads after it.
        handle.wait_ready().expect("ready");
        let registered = || {
            let hooks = handle.eval("::veles::hooks_list").unwrap_or_default();
            (
                crate::tools::global_tool_names().contains(&"ownprobe_tool".to_string()),
                crate::commands::plugin_command("ownprobe_cmd").is_some(),
                hooks.contains("ownprobe::hook"),
            )
        };
        assert_eq!(
            registered(),
            (true, true, true),
            "sourced: all three registered"
        );
        assert_eq!(
            plugin_rows(),
            vec![("ownprobe".to_string(), "loaded")],
            "listed as loaded"
        );

        let ask = |req: fn(String, oneshot::Sender<Result<String, String>>) -> TclRequest| {
            let (tx, rx) = oneshot::channel();
            handle.send(req("ownprobe".to_string(), tx)).expect("send");
            rx.blocking_recv().expect("reply")
        };
        let out = ask(|name, reply| TclRequest::Unload { name, reply }).expect("unload");
        assert!(
            out.contains("1 hook handler") && out.contains("2 registration"),
            "{out}"
        );
        assert_eq!(
            registered(),
            (false, false, false),
            "unloaded: all three gone"
        );
        assert_eq!(plugin_rows(), vec![("ownprobe".to_string(), "unloaded")]);

        // The watcher's reload re-sources every plugin — except one
        // that was unloaded.
        let (tx, rx) = oneshot::channel();
        handle.send(TclRequest::Reload { reply: tx }).expect("send");
        rx.blocking_recv().expect("reply").expect("reload");
        assert_eq!(
            registered(),
            (false, false, false),
            "a reload does not resurrect it"
        );

        let out = ask(|name, reply| TclRequest::Load { name, reply }).expect("load");
        assert!(out.contains("loaded"), "{out}");
        assert_eq!(
            registered(),
            (true, true, true),
            "loaded again: all three back"
        );
        assert_eq!(plugin_rows(), vec![("ownprobe".to_string(), "loaded")]);
        // Loading a loaded plugin is its reload: nothing doubles.
        let out = ask(|name, reply| TclRequest::Load { name, reply }).expect("reload one");
        assert!(
            out.contains("1 hook handler"),
            "its earlier handler was purged first: {out}"
        );
        let hooks = handle.eval("::veles::hooks_list").expect("hooks");
        assert_eq!(hooks.matches("ownprobe::hook").count(), 1, "{hooks}");
        assert!(
            ask(|name, reply| TclRequest::Load {
                name: format!("{name}-nope"),
                reply
            })
            .is_err(),
            "an unknown name is refused"
        );
        // Leave nothing behind for the other tests in this process.
        let _ = ask(|name, reply| TclRequest::Unload { name, reply });
    }

    /// A hook handler's `veles::tool_call` is judged as ITS OWN session,
    /// and the Tcl fast path is gated.
    ///
    /// Two bugs in one test. The bridge read the LAST-published registry,
    /// so a guest's plugin call was gated on whoever booted last — here
    /// the owner's registry is deliberately the global one, and the
    /// guest's hook must still be refused. And the fast path (a
    /// plugin-registered Tcl tool, run directly on the interpreter thread
    /// because an async dispatch from there would wedge it) never went
    /// through `dispatch` at all, so it ran with no capability check for
    /// ANY session.
    ///
    /// `wordcount` is the target twice over: it is a Tcl proc, so it
    /// takes that fast path, and nobody classified it — so the
    /// fail-closed rule makes it owner-only and the refusal needs no
    /// `[tool_caps]` line to exist.
    #[test]
    fn a_hook_handlers_tool_call_is_judged_as_its_own_session() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");
        let examples = format!("{}/../../plugins/examples", env!("CARGO_MANIFEST_DIR"));
        std::env::set_var(
            "VELES_PLUGIN_DIRS",
            format!("{examples}/tcl-wordcount,{examples}/tcl-alias-filter"),
        );
        let _clean = EnvCleaner("VELES_PLUGIN_DIRS");
        let handle = spawn(home.path().to_path_buf()).expect("spawn");
        handle.wait_ready().expect("tcl ready");

        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("state.db")).expect("store"),
        ));
        let (guest, owner) = ("hook-identity-guest", "hook-identity-owner");
        // Built AFTER the plugin load, so both registries contain
        // `wordcount` — extras flow into every registry built later.
        let guest_reg =
            std::sync::Arc::new(crate::tools::builtin_registry(store.clone(), None, guest));
        let owner_reg =
            std::sync::Arc::new(crate::tools::builtin_registry(store.clone(), None, owner));
        crate::tools::publish_session_registry(guest, guest_reg);
        crate::tools::publish_session_registry(owner, owner_reg.clone());
        // The owner booted last: this is the cell the bridge used to
        // read, and it is still what a call with no session anywhere
        // falls back to.
        crate::tools::set_global_registry(owner_reg);
        crate::acl::set_session_principal(
            guest,
            crate::acl::Principal::known("irc:stranger", crate::acl::Role::Guest),
        );

        // The handler shape every existing plugin uses: no session
        // argument, because the engine now says who is asking. Registered
        // on `pre_gateway_dispatch` because that is the hook the
        // alias-filter sample already publishes a handler for — the bus
        // skips the interpreter hop for a hook nobody registered, and the
        // registry is published by the loader, not by `veles::hook`.
        // The sample's own handler is switched off so this one answers.
        handle
            .eval(
                r#"proc ::identity_probe {} {
                       return "sid=[set ::veles::hook_session] out=[veles::tool_call wordcount {"text":"one two"}]"
                   }
                   ::veles::hook_disable pre_gateway_dispatch alias_filter::handle
                   ::veles::hook pre_gateway_dispatch ::identity_probe"#,
            )
            .expect("register probe");

        let bus = crate::hooks::HookBus::new(Some(handle.clone()));
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let fire = |sid: &str| {
            rt.block_on(bus.fire(
                "pre_gateway_dispatch",
                &crate::hooks::with_session(sid, serde_json::json!({"text": "hello"})),
            ))
            .expect("the probe must answer")
        };

        // The guest's hook: refused, and the handler can see whose it is.
        let verdict = fire(guest);
        assert!(
            verdict.contains(&format!("sid={guest}")),
            "the payload's session must reach the handler: {verdict}"
        );
        assert!(
            verdict.contains("access denied"),
            "a guest's plugin call took the fast path ungated: {verdict}"
        );

        // The owner's hook: the identical call runs.
        let verdict = fire(owner);
        assert!(
            verdict.contains(&format!("sid={owner}")),
            "the owner's hook must see its own session: {verdict}"
        );
        assert!(
            verdict.contains("words=2"),
            "the owner's call must not be refused a guest's caps: {verdict}"
        );

        // The ambient does not outlive the dispatch. A stale one would
        // judge the next caller on this thread as this session — the same
        // mistake wearing a different hat.
        assert_eq!(
            handle.eval("set ::veles::hook_session").expect("read var"),
            "",
            "::veles::hook_session must be cleared after the dispatch"
        );

        // An explicit session wins with no ambient at all — the escape
        // hatch for a handler that defers its call to a thread of its own.
        let out = handle
            .eval(&format!(
                "veles::tool_call wordcount {{\"text\":\"one two\"}} {guest}"
            ))
            .expect("a refusal is a result, not a Tcl error");
        assert!(
            out.contains("access denied"),
            "the named guest session must be judged as the guest: {out}"
        );

        // Naming a session that never booted is an ERROR, not a silent
        // fall-through to the owner's registry.
        let err = handle
            .eval("veles::tool_call wordcount {\"text\":\"x\"} hook-identity-nosuch")
            .expect_err("an unknown session must not fall through");
        assert!(err.contains("hook-identity-nosuch"), "{err}");

        // Nothing named, nothing ambient: the global cell still answers.
        let out = handle
            .eval("veles::tool_call wordcount {\"text\":\"one two\"}")
            .expect("fallback");
        assert!(out.contains("words=2"), "{out}");
    }
}
