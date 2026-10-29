//! Native FFI bridge — Tcl commands backed by real engine capabilities.
//!
//! This is what makes veles plugins *bidirectional*: plugin code doesn't
//! just receive events, it can DRIVE the engine. Registered via tcltk's
//! `def_proc_with_client_data` (Tcl_CreateObjCommand) — the same raw API
//! eggdrop modules use.
//!
//! The standing design rule (2026-09-03, plugin plan): a Tcl script
//! could in theory drive EVERYTHING — Rust exists for performance and
//! static typing in the core, not to gate capability. Every command
//! below is a real engine capability, not a facade.
//!
//! Commands (all `TCL_ERROR` on failure with a message result, never a
//! panic — panics across FFI are UB):
//!
//!   veles::call_llm <system> <user> ?provider? ?model?
//!       One-shot LLM call through the route chain (aux-task pattern —
//!       hermes' register_auxiliary_task + call_llm parity). Returns
//!       the assistant content.
//!   veles::routes
//!       The configured route chain as lines of "provider -> model".
//!   veles::sessions
//!       JSON array of session metadata (newest first): id, title,
//!       created_at, message_count.
//!   veles::history <session_id> ?limit?
//!       JSON: {"session_id", "total", "messages":[{role, content,
//!       tool_name, created_at}…]} — the LAST `limit` (default 20)
//!       messages, chronological.
//!   veles::usage_totals ?session_id?
//!       JSON token accounting, global or per-session.
//!   veles::config_get <dotted.path>
//!       Read the live config as JSON and walk the path
//!       ("agent.guards"). Missing path → "" (json_get convention).
//!   veles::config_set <dotted.path> <json-value>
//!       Write one key to disk through configedit (comments kept, .bak
//!       backup, unreadable results refused). "" / null UNSETS. Returns
//!       {"saved","backup","effect","note"} — the effect advisory
//!       (live / new_sessions / restart_gateway) is the honest answer
//!       to "does this need a restart".
//!   veles::http_get <url> / veles::http_post <url> <body> ?content_type?
//!       ?headers_json? ?timeout_secs?
//!       HTTP from the bridge runtime: 30s timeout (http_post may ask for
//!       up to 900), 1MB body cap
//!       (slopdrop parity). The plugins interp is the TRUSTED owner
//!       surface — no untrusted caller reaches these.
//!   veles::send_outbound <platform> <chat_id> <text>
//!       Push a message to a live gateway adapter (Telegram/IRC/…).
//!       Fails cleanly when no gateway is running or the platform has
//!       no adapter. Every send parks a plugin-action note — silent
//!       outbound automation is prohibited.
//!   veles::note <summary> ?detail?
//!       Park a plugin-action note for the transcript. Plugin
//!       automation must be visible (notes.rs charter).
//!   veles::tasks / veles::task_cancel <id>
//!       Background-task inspection/cancellation (tasks machinery).
//!   veles::tool_call <name> <args_json> ?session_id?
//!       Dispatch a real tool (aliases + deferred tools resolve exactly
//!       as in a turn) and return its string output. Judged as
//!       `session_id` when given, else as the session whose hook is
//!       running, else as the last-published session — and the ACL gate
//!       runs on whichever that resolves to. Called from a hook, a
//!       plugin normally passes nothing and is judged as its own caller.
//!   veles::register_tool <name> <description> <schema_json> <procname>
//!       Register a plugin tool. The proc reads `::veles::tool_args`
//!       (the JSON arguments) and returns the tool output. Rides
//!       `register_global_tool`: sessions created AFTER registration
//!       see it (documented limit, same as adapter tools).
//!   veles::register_command <name> <description> <procname>
//!       Register a slash command whose render is a proc call. The
//!       proc reads `::veles::command_args` and returns prompt text.
//!       Wins over a same-named .md file.
//!   veles::json_get <json> <key> ?key2 …?
//!       JSON field extraction (pure — no client data).
//!
//! Threading: the Tcl interpreter lives on its own std thread (not
//! inside a tokio runtime), so the bridge owns a dedicated small
//! runtime and blocks on it for async capabilities. Calls are
//! short-lived side-tasks — fine to block.
//!
//! Construction: `Bridge::new` succeeds whenever config loads. Routes
//! resolve LAZILY per call, so a provider-less boot no longer costs
//! plugins the whole bridge — sessions/history/http/tool_call all work
//! without any route configured (the pre-2026-09-03 behavior was
//! try_new → None → zero native commands).

use crate::config;
use crate::providers::{build_routes, Route};
use serde_json::Value;
use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_void};
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

pub const TCL_OK: c_int = 0;
pub const TCL_ERROR: c_int = 1;

pub struct Bridge {
    cfg: config::VelesConfig,
    rt: Runtime,
    /// Resolved once, lazily: an empty/absent provider list is a
    /// per-CALL error now, not a boot-time bridge death.
    routes: OnceLock<Result<Vec<Route>, String>>,
    store_path: std::path::PathBuf,
}

impl Bridge {
    /// Build the bridge from config. None only when config itself is
    /// unreadable — the interpreter then lacks the native commands and
    /// plugins degrade gracefully.
    pub fn new() -> Option<Arc<Bridge>> {
        let cfg = config::load().ok()?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .ok()?;
        Some(Arc::new(Bridge {
            store_path: config::veles_home().join("state.db"),
            cfg,
            rt,
            routes: OnceLock::new(),
        }))
    }

    /// The resolved route chain. `pub(crate)` because `bridgeops` is
    /// the other half of this type — the operations moved there so a
    /// second language could reach them, not because they stopped
    /// belonging to the bridge.
    pub(crate) fn routes_or_err(&self) -> Result<&Vec<Route>, String> {
        self.routes
            .get_or_init(|| build_routes(&self.cfg).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| e.clone())
    }

    pub(crate) fn cfg(&self) -> &config::VelesConfig {
        &self.cfg
    }

    /// Block the calling thread on the bridge's own small runtime.
    ///
    /// The Tcl interpreter lives on a plain std thread, so blocking is
    /// correct there. A `.so` plugin's handler is also sync by
    /// contract, and the engine wraps it in `spawn_blocking` wherever
    /// it runs one — so blocking is correct there too. What must never
    /// happen is calling this from inside an async task on the MAIN
    /// runtime; nothing does, and the 300s guard in `tool_call` is the
    /// backstop for the one shape that could wedge.
    pub(crate) fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    /// Spawn a task on the BRIDGE's runtime.
    ///
    /// Not `crate::rt::spawn`: that falls through to `tokio::spawn`, which
    /// needs an ambient runtime, and the Tcl interpreter thread has none —
    /// it is a plain std thread. The panic that produces
    /// ("there is no reactor running") is not merely loud, it is fatal: the
    /// op shims in `bridge.rs` are `extern "C"`, so a panic in one cannot
    /// unwind and the process aborts. `bridgeops`'s contract is "never
    /// panics", and this is what lets an op that must run a background task
    /// keep it.
    ///
    /// The runtime here is multi-threaded, so a task spawned through it runs
    /// independently of a `block_on` on the calling thread — which is exactly
    /// the pairing `streamhttp` needs.
    pub(crate) fn spawn<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.rt.spawn(fut)
    }

    /// A fresh connection per call: the bridge has no session of its
    /// own, and SQLite handles concurrent readers happily. Keeping the
    /// connection out of the struct also means a store migration or
    /// lock held by the main session never wedges the interpreter.
    pub(crate) fn store_conn(&self) -> Result<crate::store::Store, String> {
        crate::store::Store::open(&self.store_path).map_err(|e| e.to_string())
    }

    /// Register all native commands into the interpreter.
    ///
    /// # Safety
    /// The interpreter pointer must outlive the Arc (it does: both are
    /// owned by the Tcl thread; the Arc leaks into ClientData for the
    /// process lifetime — one bridge per interpreter).
    pub unsafe fn register(self: Arc<Self>, interp: &tcl::Interpreter) {
        let ptr = Arc::into_raw(self) as *mut c_void;
        // NOTE: Arc::into_raw consumed the refcount; every command
        // shares the same allocation, deliberately leaked for the
        // process lifetime.
        interp.def_proc_with_client_data("veles::call_llm", ffi_call_llm, ptr, None);
        interp.def_proc_with_client_data("veles::routes", ffi_routes, ptr, None);
        interp.def_proc_with_client_data("veles::sessions", ffi_sessions, ptr, None);
        interp.def_proc_with_client_data("veles::history", ffi_history, ptr, None);
        interp.def_proc_with_client_data("veles::usage_totals", ffi_usage_totals, ptr, None);
        interp.def_proc_with_client_data("veles::config_get", ffi_config_get, ptr, None);
        interp.def_proc_with_client_data("veles::config_set", ffi_config_set, ptr, None);
        interp.def_proc_with_client_data("veles::http_get", ffi_http_get, ptr, None);
        interp.def_proc_with_client_data("veles::http_post", ffi_http_post, ptr, None);
        // The streaming transport. Registered beside http_post because
        // that is what they exist to replace for a provider driver; the
        // reason they are three ops is in streamhttp's module docs.
        interp.def_proc_with_client_data(
            "veles::http_stream_open",
            ffi_http_stream_open,
            ptr,
            None,
        );
        interp.def_proc_with_client_data(
            "veles::http_stream_next",
            ffi_http_stream_next,
            ptr,
            None,
        );
        interp.def_proc_with_client_data(
            "veles::http_stream_close",
            ffi_http_stream_close,
            ptr,
            None,
        );
        interp.def_proc_with_client_data("veles::send_outbound", ffi_send_outbound, ptr, None);
        interp.def_proc_with_client_data("veles::tasks", ffi_tasks, ptr, None);
        interp.def_proc_with_client_data("veles::task_cancel", ffi_task_cancel, ptr, None);
        interp.def_proc_with_client_data("veles::tool_call", ffi_tool_call, ptr, None);
        // Plugin services (`crate::services`): use another plugin's
        // feature, list who provides what, provide one from a proc.
        interp.def_proc_with_client_data("veles::service_call", ffi_service_call, ptr, None);
        interp.def_proc_with_client_data("veles::service_next", ffi_service_next, ptr, None);
        interp.def_proc_with_client_data("veles::services", ffi_services, ptr, None);
        interp.def_proc_with_client_data(
            "veles::register_service",
            ffi_register_service,
            ptr,
            None,
        );
        interp.def_proc_with_client_data("veles::register_tool", ffi_register_tool, ptr, None);
        interp.def_proc_with_client_data(
            "veles::register_command",
            ffi_register_command,
            ptr,
            None,
        );
        interp.def_proc_with_client_data(
            "veles::register_provider",
            ffi_register_provider,
            ptr,
            None,
        );
        // `json_get` and `note` need nothing from the bridge, and used
        // to be registered with null client data to say so. They go
        // through the same `bridgeops::dispatch(&Bridge, …)` entry
        // point as everything else now, so they take the pointer like
        // everything else. One table means one signature; a
        // stateless-op shortcut here would be a second calling
        // convention to keep in step with the C ABI.
        interp.def_proc_with_client_data("veles::json_get", ffi_json_get, ptr, None);
        interp.def_proc_with_client_data("veles::note", ffi_note, ptr, None);
        interp.def_proc_with_client_data("veles::nodes", ffi_nodes, ptr, None);
        interp.def_proc_with_client_data("veles::provider_emit", ffi_provider_emit, ptr, None);
        // Reading the identity model the plugin lives in: who a session
        // acts for, where its tools run, what the budgets are. The
        // bridge could not answer any of it before.
        interp.def_proc_with_client_data("veles::principal", ffi_principal, ptr, None);
        interp.def_proc_with_client_data("veles::sandbox_where", ffi_sandbox_where, ptr, None);
        interp.def_proc_with_client_data("veles::limits", ffi_limits, ptr, None);
    }
}

// ── the process-global bridge ────────────────────────────────────────
//
// The Tcl interpreter gets its bridge handed to it: `register` leaks an
// `Arc` into ClientData and every command receives that pointer. A `.so`
// plugin's `host_call` has no such channel — it is a bare
// `extern "C" fn(op, args)` with nowhere to put a context — so it needs
// one bridge it can find by itself.
//
// Same cell pattern as `hooks::DEFAULT_BUS` and
// `tools::GLOBAL_REGISTRY`: `tclrt::spawn` publishes the bridge it just
// built, and anything asking before that gets one built on demand. In a
// normal boot the Tcl runtime starts first and there is exactly one
// bridge (one config read, one 2-worker runtime); a build with no Tcl
// at all still gives `.so` plugins a working engine.
static GLOBAL_BRIDGE: OnceLock<std::sync::RwLock<Option<Arc<Bridge>>>> = OnceLock::new();

fn global_cell() -> &'static std::sync::RwLock<Option<Arc<Bridge>>> {
    GLOBAL_BRIDGE.get_or_init(|| std::sync::RwLock::new(None))
}

/// Publish the bridge for callers with no channel of their own.
/// Idempotent: last writer wins, and there is only ever one writer.
pub fn publish_global(bridge: Arc<Bridge>) {
    if let Ok(mut w) = global_cell().write() {
        *w = Some(bridge);
    }
}

/// The process bridge, building one if nothing has published yet.
/// `None` only when config itself is unreadable — the same condition
/// under which the Tcl interpreter gets no native commands.
pub fn global() -> Option<Arc<Bridge>> {
    if let Ok(r) = global_cell().read() {
        if let Some(b) = r.as_ref() {
            return Some(Arc::clone(b));
        }
    }
    let built = Bridge::new()?;
    publish_global(Arc::clone(&built));
    Some(built)
}

fn set_result(interp: *mut clib::Tcl_Interp, s: &str) {
    let c = CString::new(s).unwrap_or_else(|_| CString::new("").unwrap());
    unsafe {
        let obj = clib::Tcl_NewStringObj(c.as_ptr(), -1);
        clib::Tcl_SetObjResult(interp, obj);
    }
}

/// Tcl's braces QUOTE a word — `{"a":1}` written in plugin source
/// arrives with the outer braces stripped (`"a":1`). JSON arriving via
/// variables (interp.set) is intact. Accept both: parse direct first,
/// then re-wrapped. This is what keeps `veles::tool_call t {"a":1}`
/// and `veles::tool_call t {{"a":1}}` spelling-compatible.
pub(crate) fn parse_json_lenient(s: &str) -> Result<Value, serde_json::Error> {
    match serde_json::from_str(s) {
        Ok(v) => Ok(v),
        Err(e) => {
            let wrapped = format!("{{{s}}}");
            serde_json::from_str(&wrapped).or(Err(e))
        }
    }
}

fn arg_str(objc: c_int, objv: *const *mut clib::Tcl_Obj, i: c_int) -> Option<String> {
    // Tcl objv is 0-based with objv[0] = command name; valid indices are
    // 0..objc. Reading past objc is UB (segfault in Tcl_GetString).
    if i >= objc {
        return None;
    }
    unsafe {
        let p = *objv.add(i as usize);
        if p.is_null() {
            return None;
        }
        let cs = CStr::from_ptr(clib::Tcl_GetString(p));
        Some(cs.to_string_lossy().into_owned())
    }
}

/// Common FFI prelude: fetch the bridge or fail with a message.
/// Safety: the pointer came from Arc::into_raw in `register` and lives
/// for the process lifetime (one bridge per interpreter).
macro_rules! bridge_or_err {
    ($client_data:expr, $interp:expr) => {
        match unsafe { ($client_data as *const Bridge).as_ref() } {
            Some(b) => b,
            None => {
                set_result($interp, "bridge unavailable");
                return TCL_ERROR;
            }
        }
    };
}

/// Collect `objv[1..objc]` as owned strings.
fn arg_vec(objc: c_int, objv: *const *mut clib::Tcl_Obj) -> Option<Vec<String>> {
    (1..objc).map(|i| arg_str(objc, objv, i)).collect()
}

/// Define a Tcl command that is nothing but an adapter over one
/// `bridgeops` entry.
///
/// Every one of these used to be a 40-to-100-line function holding the
/// arity check, the work and the result-setting together, which is why
/// nothing that was not a Tcl interpreter could reach any of it (see
/// bridgeops' header for what that cost the `.so` ABI). What is left
/// here is the part that IS Tcl: pulling `Tcl_Obj`s into strings, and
/// turning a `Result` into `TCL_OK`/`TCL_ERROR` with the message in the
/// interpreter result.
macro_rules! op_command {
    ($fname:ident, $op:literal) => {
        extern "C" fn $fname(
            client_data: *mut c_void,
            interp: *mut clib::Tcl_Interp,
            objc: c_int,
            objv: *const *mut clib::Tcl_Obj,
        ) -> c_int {
            let bridge = bridge_or_err!(client_data, interp);
            let Some(args) = arg_vec(objc, objv) else {
                return TCL_ERROR;
            };
            match crate::bridgeops::dispatch(bridge, $op, &args) {
                Ok(out) => {
                    set_result(interp, &out);
                    TCL_OK
                }
                Err(e) => {
                    set_result(interp, &e);
                    TCL_ERROR
                }
            }
        }
    };
}

op_command!(ffi_call_llm, "call_llm");
op_command!(ffi_routes, "routes");
op_command!(ffi_sessions, "sessions");
op_command!(ffi_history, "history");
op_command!(ffi_usage_totals, "usage_totals");
op_command!(ffi_config_get, "config_get");
op_command!(ffi_config_set, "config_set");
op_command!(ffi_http_get, "http_get");
op_command!(ffi_http_post, "http_post");
op_command!(ffi_http_stream_open, "http_stream_open");
op_command!(ffi_http_stream_next, "http_stream_next");
op_command!(ffi_http_stream_close, "http_stream_close");
op_command!(ffi_send_outbound, "send_outbound");
op_command!(ffi_tasks, "tasks");
op_command!(ffi_task_cancel, "task_cancel");
op_command!(ffi_note, "note");
op_command!(ffi_nodes, "nodes");
op_command!(ffi_provider_emit, "provider_emit");
op_command!(ffi_json_get, "json_get");
op_command!(ffi_principal, "principal");
op_command!(ffi_sandbox_where, "sandbox_where");
op_command!(ffi_limits, "limits");
op_command!(ffi_services, "services");

/// The variable a Tcl service proc reads its call from: JSON
/// `{"params": …, "caller": …, "session_id": …}`. Read it FIRST: a
/// service the proc calls in turn sets it again.
pub const SERVICE_REQUEST_VAR: &str = "::veles::service_request";

/// A Tcl proc's answer as a service's JSON: JSON when it parses as JSON,
/// else the text as a string.
fn tcl_service_answer(out: &str) -> Value {
    match serde_json::from_str::<Value>(out.trim()) {
        Ok(v) => v,
        Err(_) => Value::String(out.to_string()),
    }
}

/// The handler a Tcl proc provides a service with: every non-Tcl caller
/// hops to the interpreter thread with the request in
/// `::veles::service_request`. It captures no interp pointer, so a
/// reload's new proc answers.
pub(crate) fn tcl_service_handler(procname: &str) -> crate::services::Handler {
    let proc_for_call = procname.to_string();
    crate::services::handler(move |params, cx| {
        let procname = proc_for_call.clone();
        async move {
            let payload = crate::services::script_payload(&params, &cx).to_string();
            let out = crate::rt::spawn_blocking(move || {
                let handle = crate::tclrt::global_handle()
                    .ok_or_else(|| "tcl runtime not running".to_string())?;
                handle.set_eval_from_blocking_thread(SERVICE_REQUEST_VAR, &payload, &procname)
            })
            .await
            .map_err(|e| format!("the Tcl service call was cancelled: {e}"))??;
            Ok(tcl_service_answer(&out))
        }
    })
}

/// `veles::register_service <name> <procname> ?description? ?cap?` — a
/// Tcl plugin PROVIDING a service other plugins call (`crate::services`).
/// The proc reads [`SERVICE_REQUEST_VAR`] and returns its answer (JSON,
/// or text). `cap` is who may call: unset = core and in-process modules
/// only, `any` = everyone, or an ACL capability the calling session's
/// principal must hold. Owned by the plugin being sourced: its unload
/// withdraws it.
extern "C" fn ffi_register_service(
    client_data: *mut c_void,
    interp: *mut clib::Tcl_Interp,
    objc: c_int,
    objv: *const *mut clib::Tcl_Obj,
) -> c_int {
    let _bridge = bridge_or_err!(client_data, interp);
    let Some(args) = arg_vec(objc, objv) else {
        return TCL_ERROR;
    };
    if !(2..=4).contains(&args.len()) {
        set_result(
            interp,
            "usage: veles::register_service <name> <procname> ?description? ?cap?",
        );
        return TCL_ERROR;
    }
    let (name, procname) = (args[0].clone(), args[1].clone());
    if !proc_exists(interp, &procname) {
        set_result(interp, &format!("proc `{procname}` is not defined"));
        return TCL_ERROR;
    }
    let decl = crate::services::ServiceDecl {
        name: name.clone(),
        description: args.get(2).cloned().unwrap_or_default(),
        cap: args.get(3).filter(|c| !c.trim().is_empty()).cloned(),
    };
    let handler = tcl_service_handler(&procname);
    match crate::services::register_tcl(decl, &procname, handler) {
        Ok(()) => {
            crate::notes::record_plugin_note(
                &format!("plugin registered service `{name}`"),
                "other plugins can call it (veles::service_call); the proc reads \
                 ::veles::service_request",
            );
            set_result(
                interp,
                &format!("registered service {name} (proc {procname})"),
            );
            TCL_OK
        }
        Err(e) => {
            set_result(interp, &e);
            TCL_ERROR
        }
    }
}

/// `veles::service_call <name> <params_json> ?session_id?` — with the
/// same FAST PATH `veles::tool_call` has: when a Tcl plugin provides the
/// service, THIS thread is its interpreter's, and the handler's hop to it
/// would wait for itself. So a Tcl-provided service runs here, after the
/// same gate and announcement; everything else is the shared op.
extern "C" fn ffi_service_call(
    client_data: *mut c_void,
    interp: *mut clib::Tcl_Interp,
    objc: c_int,
    objv: *const *mut clib::Tcl_Obj,
) -> c_int {
    let bridge = bridge_or_err!(client_data, interp);
    let Some(args) = arg_vec(objc, objv) else {
        return TCL_ERROR;
    };
    if args.len() == 2 || args.len() == 3 {
        if let Some(procname) = crate::services::tcl_proc(&args[0]) {
            let params = match parse_json_lenient(&args[1]) {
                Ok(v) => v,
                Err(e) => {
                    set_result(interp, &format!("bad params json: {e}"));
                    return TCL_ERROR;
                }
            };
            let caller = crate::lifecycle::thread_owner().unwrap_or("a plugin");
            let session_id =
                match crate::services::admit_plugin_call(caller, &args[0], args.get(2).cloned()) {
                    Ok(s) => s,
                    Err(e) => {
                        set_result(interp, &e);
                        return TCL_ERROR;
                    }
                };
            // The proc runs here, so its `next` token is issued here too,
            // and lives until the proc returns.
            let (next, _next_guard) =
                match crate::services::next_for_top(&args[0], caller, session_id.clone()) {
                    Some((token, guard)) => (Some(token), Some(guard)),
                    None => (None, None),
                };
            let cx = crate::services::CallCx {
                caller: caller.to_string(),
                session_id,
                next,
            };
            let payload = crate::services::script_payload(&params, &cx).to_string();
            if !set_interp_var(interp, SERVICE_REQUEST_VAR, &payload) {
                set_result(
                    interp,
                    "cannot set ::veles::service_request (nul in params?)",
                );
                return TCL_ERROR;
            }
            return match eval_on_interp(interp, &procname) {
                Ok(s) => {
                    set_result(interp, &s);
                    TCL_OK
                }
                Err(e) => {
                    set_result(interp, &format!("service `{}` failed: {e}", args[0]));
                    TCL_ERROR
                }
            };
        }
    }
    match crate::bridgeops::dispatch(bridge, "service_call", &args) {
        Ok(out) => {
            set_result(interp, &out);
            TCL_OK
        }
        Err(e) => {
            set_result(interp, &e);
            TCL_ERROR
        }
    }
}

/// `veles::service_next <token> <params_json>` — what the layer BELOW
/// this provider answers (PLAN-extension-api.md D2): a Tcl service proc
/// wrapping what it covers. `token` is the `next` field of
/// `::veles::service_request`, valid while that request runs. When the
/// layer below is itself a Tcl proc, it runs HERE (this thread is its
/// interpreter's, and the op's hop to it would wait for itself), with the
/// caller's own `::veles::service_request` put back afterwards; anything
/// else is the shared op.
extern "C" fn ffi_service_next(
    client_data: *mut c_void,
    interp: *mut clib::Tcl_Interp,
    objc: c_int,
    objv: *const *mut clib::Tcl_Obj,
) -> c_int {
    let bridge = bridge_or_err!(client_data, interp);
    let Some(args) = arg_vec(objc, objv) else {
        return TCL_ERROR;
    };
    if args.len() == 2 {
        if let Some(procname) = crate::services::tcl_proc_below(&args[0]) {
            let params = match parse_json_lenient(&args[1]) {
                Ok(v) => v,
                Err(e) => {
                    set_result(interp, &format!("bad params json: {e}"));
                    return TCL_ERROR;
                }
            };
            let (payload, _next_guard) =
                match crate::services::next_payload_below(&args[0], &params) {
                    Ok(p) => p,
                    Err(e) => {
                        set_result(interp, &e);
                        return TCL_ERROR;
                    }
                };
            let saved = eval_on_interp(interp, &format!("set {SERVICE_REQUEST_VAR}")).ok();
            if !set_interp_var(interp, SERVICE_REQUEST_VAR, &payload.to_string()) {
                set_result(
                    interp,
                    "cannot set ::veles::service_request (nul in params?)",
                );
                return TCL_ERROR;
            }
            let answer = eval_on_interp(interp, &procname);
            if let Some(s) = saved {
                set_interp_var(interp, SERVICE_REQUEST_VAR, &s);
            }
            return match answer {
                Ok(s) => {
                    set_result(interp, &s);
                    TCL_OK
                }
                Err(e) => {
                    set_result(interp, &format!("the layer below failed: {e}"));
                    TCL_ERROR
                }
            };
        }
    }
    match crate::bridgeops::dispatch(bridge, "service_next", &args) {
        Ok(out) => {
            set_result(interp, &out);
            TCL_OK
        }
        Err(e) => {
            set_result(interp, &e);
            TCL_ERROR
        }
    }
}

/// `veles::tool_call <name> <args_json> ?session_id?` — the one op with
/// a Tcl-side prologue rather than a bare adapter.
///
/// FAST PATH: when the target tool is itself a Tcl proc, THIS thread is
/// its interpreter's thread. `bridgeops::tool_call` goes through the
/// async registry, whose `TclTool::call` posts an Eval request to the
/// very thread waiting on it — a permanent wedge. So a Tcl-backed tool
/// runs here, directly, and everything else falls through to the
/// shared implementation.
///
/// Running it here means gating it here: `ToolRegistry::gate` is the
/// same check `dispatch` makes, and this path skipped it entirely until
/// it was added — a plugin-registered Tcl tool ran with no capability
/// check for any session. Identity resolution is shared too
/// (`tools::plugin_call_registry`), so the two paths cannot answer
/// differently about WHOSE call it is.
extern "C" fn ffi_tool_call(
    client_data: *mut c_void,
    interp: *mut clib::Tcl_Interp,
    objc: c_int,
    objv: *const *mut clib::Tcl_Obj,
) -> c_int {
    let bridge = bridge_or_err!(client_data, interp);
    let Some(args) = arg_vec(objc, objv) else {
        return TCL_ERROR;
    };
    if args.len() == 2 || args.len() == 3 {
        // Same question, same answer as the shared op: whose call is
        // this? Resolved here because the fast path runs the tool itself
        // and must therefore gate it itself.
        let explicit = args.get(2).map(String::as_str);
        let ambient = crate::tools::ambient_hook_session();
        match crate::tools::plugin_call_registry(explicit, ambient.as_deref()) {
            Err(e) => {
                set_result(interp, &e);
                return TCL_ERROR;
            }
            Ok((registry, _judged_as)) => {
                let target = registry.resolve_alias(&args[0]);
                // Not a Tcl-proc tool: nothing to run here, and every
                // arm below returns — so falling out of this block lands
                // on the shared implementation, which resolves the same
                // identity again and dispatches asynchronously.
                match registry
                    .get(&target)
                    .and_then(|t| t.tcl_proc().map(str::to_string))
                {
                    None => {}
                    Some(procname) => {
                        let parsed = match parse_json_lenient(&args[1]) {
                            Ok(v) => v,
                            Err(e) => {
                                set_result(interp, &format!("bad args json: {e}"));
                                return TCL_ERROR;
                            }
                        };
                        // ── THE GATE ──
                        // `ToolRegistry::dispatch` has always run this
                        // check for every caller that reaches it. This
                        // path never did: a plugin-registered Tcl tool ran
                        // with no capability check at all, for any session
                        // — the one caller with no gate was the one an
                        // untrusted plugin reaches without going through
                        // the model. Same function, same resolved TARGET,
                        // so the two paths cannot drift.
                        //
                        // A denial comes back as the RESULT (TCL_OK), not
                        // as a Tcl error: that is what the async path does
                        // (`dispatch` returns the denial string), and a
                        // plugin reads its tool's output either way.
                        if let Some(denial) = registry.gate(&target) {
                            set_result(interp, &denial);
                            return TCL_OK;
                        }
                        if !set_interp_var(interp, "::veles::tool_args", &parsed.to_string()) {
                            set_result(interp, "cannot set ::veles::tool_args (nul in args?)");
                            return TCL_ERROR;
                        }
                        return match eval_on_interp(interp, &procname) {
                            Ok(s) => {
                                set_result(interp, &s);
                                TCL_OK
                            }
                            Err(e) => {
                                set_result(interp, &format!("tool `{}` failed: {e}", args[0]));
                                TCL_ERROR
                            }
                        };
                    }
                }
            }
        }
    }
    match crate::bridgeops::dispatch(bridge, "tool_call", &args) {
        Ok(out) => {
            set_result(interp, &out);
            TCL_OK
        }
        Err(e) => {
            set_result(interp, &e);
            TCL_ERROR
        }
    }
}
/// Set a variable VERBATIM on the interpreter THIS FFI call runs on
/// (Tcl_SetVar — the value is data, never re-parsed, so JSON survives
/// intact; brace-quoting through an eval string would not).
fn set_interp_var(interp: *mut clib::Tcl_Interp, var: &str, value: &str) -> bool {
    let (Ok(var_c), Ok(val_c)) = (CString::new(var), CString::new(value)) else {
        return false;
    };
    unsafe {
        clib::Tcl_SetVar(interp, var_c.as_ptr(), val_c.as_ptr(), 0);
    }
    true
}

/// Evaluate a (possibly multi-command) script directly on the
/// interpreter THIS FFI call runs on. For the bridge's Tcl-tool fast
/// path: same thread, no channel round-trip, no deadlock.
fn eval_on_interp(interp: *mut clib::Tcl_Interp, code: &str) -> Result<String, String> {
    let c = match CString::new(code) {
        Ok(c) => c,
        Err(_) => return Err("nul byte in eval code".into()),
    };
    unsafe {
        let rc = clib::Tcl_Eval(interp, c.as_ptr());
        let res = clib::Tcl_GetStringResult(interp);
        let s = if res.is_null() {
            String::new()
        } else {
            CStr::from_ptr(res).to_string_lossy().into_owned()
        };
        if rc == TCL_OK {
            Ok(s)
        } else {
            Err(s)
        }
    }
}

/// A tool whose implementation is a proc in the plugins interpreter.
/// `call` runs on the agent's tokio runtime, so it must NEVER block —
/// it sends an Eval request and awaits the oneshot reply.
pub struct TclTool {
    name: String,
    description: String,
    schema: Value,
    procname: String,
}

#[async_trait::async_trait]
impl crate::tools::Tool for TclTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    fn tcl_proc(&self) -> Option<&str> {
        Some(&self.procname)
    }
    async fn call(&self, args: Value) -> String {
        let Some(handle) = crate::tclrt::global_handle() else {
            return "error: tcl runtime not running".into();
        };
        // Args ride a VERBATIM variable (SetEval — JSON survives; Tcl
        // brace quoting would not); the proc call is one atomic request
        // on the serialized Tcl thread, so concurrent tool calls cannot
        // interleave.
        let (tx, rx) = tokio::sync::oneshot::channel();
        if handle
            .send(crate::tclrt::TclRequest::SetEval {
                var: "::veles::tool_args".into(),
                value: args.to_string(),
                code: self.procname.clone(),
                reply: tx,
            })
            .is_err()
        {
            return "error: tcl thread dead".into();
        }
        match rx.await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => format!("error: {e}"),
            Err(_) => "error: tcl thread dropped".into(),
        }
    }
}

/// True when `procname` names a proc the interpreter actually has.
/// Runs on the Tcl thread (FFI context), so a direct eval is right.
fn proc_exists(interp: *mut clib::Tcl_Interp, procname: &str) -> bool {
    let check = format!("info commands {}", crate::tclrt::quote(procname));
    let c = match CString::new(check) {
        Ok(c) => c,
        Err(_) => return false,
    };
    unsafe {
        let code = clib::Tcl_Eval(interp, c.as_ptr());
        if code != TCL_OK {
            return false;
        }
        let res = clib::Tcl_GetStringResult(interp);
        !res.is_null() && !CStr::from_ptr(res).to_string_lossy().trim().is_empty()
    }
}

/// Tool/command name sanity: ascii letters/digits/_/- (registry
/// discipline — weird names break schema serialization and ACL keys).
fn valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

extern "C" fn ffi_register_tool(
    client_data: *mut c_void,
    interp: *mut clib::Tcl_Interp,
    objc: c_int,
    objv: *const *mut clib::Tcl_Obj,
) -> c_int {
    // veles::register_tool <name> <description> <schema_json> <procname>
    if objc != 5 {
        set_result(
            interp,
            "usage: veles::register_tool <name> <description> <schema_json> <procname>",
        );
        return TCL_ERROR;
    }
    let _bridge = bridge_or_err!(client_data, interp);
    let name = match arg_str(objc, objv, 1) {
        Some(s) => s,
        None => return TCL_ERROR,
    };
    let description = match arg_str(objc, objv, 2) {
        Some(s) => s,
        None => return TCL_ERROR,
    };
    let schema_json = match arg_str(objc, objv, 3) {
        Some(s) => s,
        None => return TCL_ERROR,
    };
    let procname = match arg_str(objc, objv, 4) {
        Some(s) => s,
        None => return TCL_ERROR,
    };
    if !valid_plugin_name(&name) {
        set_result(
            interp,
            "tool name must be ascii letters/digits/_/- (max 64 chars)",
        );
        return TCL_ERROR;
    }
    // Tcl's braces QUOTE a word — `{"type":"object"}` arrives with the
    // outer braces stripped (see parse_json_lenient). Accept both
    // spellings; the result must be an object either way.
    let schema: Value = match parse_json_lenient(&schema_json) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => {
            set_result(interp, "schema must be a JSON object");
            return TCL_ERROR;
        }
        Err(e) => {
            set_result(interp, &format!("bad schema json: {e}"));
            return TCL_ERROR;
        }
    };
    // Refuse to register a tool whose proc does not exist: calling it
    // later would fail with a bare Tcl error and no pointer here.
    if !proc_exists(interp, &procname) {
        set_result(interp, &format!("proc `{procname}` is not defined"));
        return TCL_ERROR;
    }
    crate::tools::register_global_tool(Arc::new(TclTool {
        name: name.clone(),
        description,
        schema,
        procname: procname.clone(),
    }));
    crate::notes::record_plugin_note(
        &format!("plugin registered tool `{name}`"),
        "sessions created after registration can call it; the proc reads ::veles::tool_args",
    );
    set_result(interp, &format!("registered tool {name} (proc {procname})"));
    TCL_OK
}

/// `veles::register_provider <name> <procname>` — a Tcl plugin supplying
/// a WIRE.
///
/// Tcl-only for the same reason `register_tool` and `register_command`
/// are: the last argument is a proc name. The `.so` equivalent takes a
/// function pointer.
///
/// The proc reads `::veles::provider_request` (the OpenAI-shaped request
/// body the engine built, so the driver translates rather than
/// assembles), may call `veles::provider_emit` any number of times to
/// stream, and returns an OpenAI-shaped response object — which
/// `completion_from_wire` turns into a `Completion`, so tool-call
/// reassembly, the reasoning/content split and every spelling of cache
/// usage come for free.
extern "C" fn ffi_register_provider(
    client_data: *mut c_void,
    interp: *mut clib::Tcl_Interp,
    objc: c_int,
    objv: *const *mut clib::Tcl_Obj,
) -> c_int {
    if objc != 3 {
        set_result(interp, "usage: veles::register_provider <name> <procname>");
        return TCL_ERROR;
    }
    let _bridge = bridge_or_err!(client_data, interp);
    let Some(args) = arg_vec(objc, objv) else {
        return TCL_ERROR;
    };
    let (name, procname) = (args[0].clone(), args[1].clone());
    if !valid_plugin_name(&name) {
        set_result(interp, "provider driver name must be letters/digits/_/-");
        return TCL_ERROR;
    }
    if !proc_exists(interp, &procname) {
        set_result(interp, &format!("proc `{procname}` is not defined"));
        return TCL_ERROR;
    }

    // The driver closure hops to the interpreter thread. It does NOT
    // capture this interp pointer: a reload replaces the proc, and the
    // handle resolves the live interpreter each call, so a driver keeps
    // working across reloads instead of calling into a stale proc.
    let proc_for_call = procname.clone();
    let call: crate::providers::ProviderDriverFn = Arc::new(
        move |body: &Value, _emit: &mut dyn FnMut(&Value)| -> Result<Value, String> {
            let handle = crate::tclrt::global_handle()
                .ok_or_else(|| "tcl runtime not running".to_string())?;
            // Chunks do not come back through `_emit`: the proc runs on
            // another thread and reaches the in-flight call through
            // `veles::provider_emit`, which is why that is an op rather
            // than a callback threaded through here.
            let out = handle.set_eval_from_blocking_thread(
                "::veles::provider_request",
                &body.to_string(),
                &proc_for_call,
            )?;
            if out.trim().is_empty() {
                return Err(format!(
                    "driver proc `{proc_for_call}` returned nothing — a driver must answer \
                     an OpenAI-shaped response object (or {{\"error\":\"...\"}})"
                ));
            }
            parse_json_lenient(&out)
                .map_err(|e| format!("driver proc `{proc_for_call}` did not answer JSON: {e}"))
        },
    );

    if let Err(e) = crate::providers::register_driver(crate::providers::ProviderDriver {
        name: name.clone(),
        plugin: format!("tcl:{procname}"),
        call,
    }) {
        set_result(interp, &e);
        return TCL_ERROR;
    }
    crate::notes::record_plugin_note(
        &format!("plugin registered provider driver `{name}`"),
        "reach it with api_mode = \"plugin\" and driver = \"<name>\" on a provider entry",
    );
    set_result(
        interp,
        &format!("registered provider driver {name} (proc {procname})"),
    );
    TCL_OK
}
extern "C" fn ffi_register_command(
    client_data: *mut c_void,
    interp: *mut clib::Tcl_Interp,
    objc: c_int,
    objv: *const *mut clib::Tcl_Obj,
) -> c_int {
    // veles::register_command <name> <description> <procname>
    if objc != 4 {
        set_result(
            interp,
            "usage: veles::register_command <name> <description> <procname>",
        );
        return TCL_ERROR;
    }
    let _bridge = bridge_or_err!(client_data, interp);
    let name = match arg_str(objc, objv, 1) {
        Some(s) => s,
        None => return TCL_ERROR,
    };
    let description = match arg_str(objc, objv, 2) {
        Some(s) => s,
        None => return TCL_ERROR,
    };
    let procname = match arg_str(objc, objv, 3) {
        Some(s) => s,
        None => return TCL_ERROR,
    };
    if !crate::commands::valid_command_name(&name) {
        set_result(interp, "command name must be letters/digits/-/_");
        return TCL_ERROR;
    }
    if !proc_exists(interp, &procname) {
        set_result(interp, &format!("proc `{procname}` is not defined"));
        return TCL_ERROR;
    }
    if let Err(e) = crate::commands::register_command(crate::commands::PluginCommand::tcl(
        &name,
        &description,
        &procname,
    )) {
        set_result(interp, &e);
        return TCL_ERROR;
    }
    crate::notes::record_plugin_note(
        &format!("plugin registered command /{name}"),
        "the proc reads ::veles::command_args and returns the prompt text",
    );
    set_result(
        interp,
        &format!("registered command /{name} (proc {procname})"),
    );
    TCL_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ChatMessage;
    use serde_json::json;

    /// The real boot path against a scratch home: empty config.toml so
    /// the native config branch wins (no hermes-config fallback leaking
    /// real routes into the test), then `tclrt::spawn` — which registers
    /// the bridge, parks the global handle, and gives us the same
    /// interpreter plugins get.
    fn boot() -> (
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        crate::tclrt::TclHandle,
    ) {
        let (home, guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("empty config");
        let handle = crate::tclrt::spawn(home.path().to_path_buf()).expect("tcl spawn");
        (home, guard, handle)
    }

    /// THE regression: before 2026-09-03 a provider-less config meant
    /// `try_new()` → None → plugins had NO native commands at all. The
    /// bridge must now exist without routes; only route-dependent calls
    /// fail, per-call.
    #[test]
    fn bridge_boots_without_routes() {
        let (_home, _g, handle) = boot();
        let out = handle
            .eval("info commands veles::*")
            .expect("info commands");
        for cmd in [
            "veles::sessions",
            "veles::history",
            "veles::usage_totals",
            "veles::config_get",
            "veles::config_set",
            "veles::http_get",
            "veles::http_post",
            "veles::send_outbound",
            "veles::note",
            "veles::tasks",
            "veles::task_cancel",
            "veles::tool_call",
            "veles::register_tool",
            "veles::register_command",
        ] {
            assert!(out.contains(cmd), "{cmd} missing from {out}");
        }
        let err = handle
            .eval("catch {veles::routes} e; set e")
            .expect("catch");
        assert!(err.contains("no routes"), "{err}");
        let err = handle
            .eval("catch {veles::call_llm sys user} e; set e")
            .expect("catch");
        assert!(err.contains("no routes"), "{err}");
    }

    #[test]
    fn sessions_history_usage_read_the_store() {
        let (home, _g, handle) = boot();
        let store = crate::store::Store::open(&home.path().join("state.db")).expect("store");
        store.create_session("bridge_test").expect("session");
        store
            .append_message(
                "bridge_test",
                &ChatMessage::user("hello from the bridge test"),
            )
            .expect("message");
        drop(store);

        let out = handle.eval("veles::sessions").expect("sessions");
        assert!(out.contains("bridge_test"), "{out}");

        let out = handle.eval("veles::history bridge_test").expect("history");
        assert!(
            out.contains("hello from the bridge test") && out.contains("\"total\":1"),
            "{out}"
        );
        // limit is honored
        let out = handle
            .eval("veles::history bridge_test 1")
            .expect("history limit");
        assert!(out.contains("hello"), "{out}");

        let out = handle.eval("veles::usage_totals").expect("usage");
        assert!(out.contains("\"calls\":"), "{out}");

        let err = handle
            .eval("veles::history no_such_session")
            .expect("an empty session is not an error");
        assert!(err.contains("\"total\":0"), "{err}");
    }

    #[test]
    fn config_get_walks_paths_and_misses_cleanly() {
        let (_home, _g, handle) = boot();
        // The new [sandbox] section defaults are live config.
        let out = handle
            .eval("veles::config_get sandbox.backend")
            .expect("get");
        assert_eq!(out, "native");
        // json_get convention: a missing path is "" + TCL_OK, not error.
        let out = handle
            .eval("veles::config_get no.such.path")
            .expect("missing path is ok");
        assert_eq!(out, "");
    }

    #[test]
    fn config_set_writes_to_disk_and_advises_the_effect() {
        let (home, _g, handle) = boot();
        // Seed pre-existing content (with a comment) so the rewrite has
        // something to PRESERVE and something to back up.
        std::fs::write(
            home.path().join("config.toml"),
            "# keep me\n[model]\ndefault = \"seeded\"\n",
        )
        .expect("seed config");
        // Typed JSON value keeps its type; the report names the effect.
        let out = handle
            .eval("veles::config_set sandbox.memory_mb 128")
            .expect("config_set");
        let report: Value = serde_json::from_str(&out).expect("report is JSON: {out}");
        // SandboxSection is OnceLock-cached: honest advisory is restart,
        // not the new_sessions default.
        assert_eq!(report["effect"], "restart_gateway", "{report}");
        assert!(
            report["note"].as_str().unwrap_or("").contains("restart"),
            "{report}"
        );
        // The file on disk carries the TYPED value (not "128" as a
        // string).
        let raw = std::fs::read_to_string(home.path().join("config.toml")).expect("config.toml");
        assert!(raw.contains("memory_mb = 128"), "{raw}");
        // …while config_get still shows the BOOT SNAPSHOT — the running
        // process keeps what it started with. That is precisely what the
        // restart_gateway advisory above means; a read-back that silently
        // showed 128 would be the lie.
        let back = handle
            .eval("veles::config_get sandbox.memory_mb")
            .expect("read back");
        assert_eq!(back, "256", "the snapshot is unchanged until restart");
        // A backup landed before the rewrite, carrying the OLD content,
        // and the comment survived the TOML-aware edit.
        assert!(home.path().join("config.toml.bak").exists());
        let bak = std::fs::read_to_string(home.path().join("config.toml.bak")).expect("backup");
        assert!(
            bak.contains("seeded") && !bak.contains("memory_mb"),
            "{bak}"
        );
        assert!(raw.contains("# keep me"), "comments are preserved: {raw}");
        assert!(
            raw.contains("default = \"seeded\""),
            "untouched keys stay: {raw}"
        );

        // "" unsets the key.
        handle
            .eval("veles::config_set sandbox.memory_mb {}")
            .expect("unset");
        let raw = std::fs::read_to_string(home.path().join("config.toml")).expect("config.toml");
        assert!(!raw.contains("memory_mb"), "unset removes the key: {raw}");

        // A malformed path fails CLEANLY (Tcl error, file untouched).
        let err = handle
            .eval("catch {veles::config_set a..b 1} e; set e")
            .expect("catch");
        assert!(err.contains("empty path segment"), "{err}");
    }

    #[test]
    fn note_parks_a_plugin_action_note() {
        let (_home, _g, handle) = boot();
        handle
            .eval("veles::note {did a thing} {the full story}")
            .expect("note");
        let notes = crate::notes::take_plugin_notes();
        assert!(
            notes
                .iter()
                .any(|n| n.summary == "did a thing" && n.detail == "the full story"),
            "the parked note must surface: {notes:?}"
        );
    }

    #[test]
    fn register_command_wins_and_renders() {
        let (_home, _g, handle) = boot();
        handle
            .eval("proc bridge_test_cmd {} { return \"prompt: $::veles::command_args\" }")
            .expect("proc");
        let out = handle
            .eval("veles::register_command bridgeecho {test command} bridge_test_cmd")
            .expect("register");
        assert!(out.contains("registered command /bridgeecho"), "{out}");

        let resolved = crate::commands::find_any("bridgeecho").expect("find_any");
        match resolved {
            crate::commands::ResolvedCommand::Plugin(t) => {
                let s = crate::commands::render_plugin_blocking(&t, "hi there").expect("render");
                assert!(s.contains("prompt: hi there"), "{s}");
            }
            _ => panic!("expected a Tcl-registered command"),
        }

        // Registering against a missing proc is refused, not deferred.
        let err = handle
            .eval("catch {veles::register_command badone desc no_such_proc} e; set e")
            .expect("catch");
        assert!(err.contains("not defined"), "{err}");
    }

    #[test]
    fn register_tool_then_dispatch_it() {
        let (home, _g, handle) = boot();
        handle
            .eval("proc bridge_wc {} { return \"chars: [string length $::veles::tool_args]\" }")
            .expect("proc");
        let out = handle
            .eval(
                r#"veles::register_tool bridge_wc_test "counts argument chars" {"type":"object","properties":{}} bridge_wc"#,
            )
            .expect("register tool");
        assert!(out.contains("registered tool bridge_wc_test"), "{out}");

        // Build the registry AFTER registration (extras flow into
        // registries constructed later) and publish it exactly as
        // Agent::new does.
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("state.db")).expect("store"),
        ));
        let reg = crate::tools::builtin_registry(store, None, "__bridge_test__");
        crate::tools::set_global_registry(Arc::new(reg));

        let out = handle
            .eval(r#"veles::tool_call bridge_wc_test {"a":1}"#)
            .expect("tool_call");
        assert!(out.contains("chars:"), "{out}");
    }

    /// A Tcl proc provides a service; a Tcl caller reaches it on its own
    /// thread (the fast path: the hop to the interpreter would wait for
    /// itself), with the request in `::veles::service_request`. An
    /// internal service refuses the plugin caller; `services` lists both.
    #[test]
    fn a_tcl_proc_provides_a_service_tcl_can_call() {
        let (_home, _g, handle) = boot();
        handle
            .eval("proc bridge_svc_echo {} { return $::veles::service_request }")
            .expect("proc");
        let out = handle
            .eval(r#"veles::register_service bridgetest.echo bridge_svc_echo "echoes" any"#)
            .expect("register");
        assert!(out.contains("registered service bridgetest.echo"), "{out}");
        let out = handle
            .eval(r#"veles::service_call bridgetest.echo {"x":1}"#)
            .expect("service_call");
        let v: Value = serde_json::from_str(&out).expect("the request, as JSON");
        assert_eq!(v["params"]["x"], 1, "{out}");
        assert!(v["caller"].is_string(), "{out}");

        handle
            .eval("veles::register_service bridgetest.internal bridge_svc_echo")
            .expect("register internal");
        let err = handle
            .eval(r#"catch {veles::service_call bridgetest.internal {}} e; set e"#)
            .expect("catch");
        assert!(err.contains("internal service"), "{err}");

        let listing = handle.eval("veles::services").expect("services");
        let rows: Value = serde_json::from_str(&listing).expect("json");
        let names: Vec<&str> = rows
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["name"].as_str())
            .collect();
        assert!(names.contains(&"bridgetest.echo"), "{listing}");
        assert!(names.contains(&"bridgetest.internal"), "{listing}");
        assert!(crate::services::tcl_proc("bridgetest.echo").is_some());

        let err = handle
            .eval("catch {veles::register_service bridgetest.nope no_such_proc} e; set e")
            .expect("catch");
        assert!(err.contains("is not defined"), "{err}");
    }

    /// `veles::service_next` (PLAN-extension-api.md D2): a Tcl layer over
    /// a Rust default reaches it through the op; a Tcl layer over another
    /// Tcl layer reaches it IN PLACE (the op's hop to the interpreter
    /// would wait for itself), and its own `::veles::service_request` is
    /// put back afterwards. Each layer appends its tag to `via`.
    #[test]
    fn a_tcl_override_wraps_next_over_rust_and_over_another_tcl_layer() {
        let (_home, _g, handle) = boot();
        let decl = |name: &str| crate::services::ServiceDecl {
            name: name.into(),
            description: String::new(),
            cap: None,
        };
        crate::lifecycle::with_thread_owner("module:bridgetest-pt", || {
            crate::services::declare_point(
                crate::services::PointDecl {
                    name: "bridgetest.point".into(),
                    summary: "the bridge test's point".into(),
                    input: json!({"type": "object"}),
                    output: json!({"type": "object", "required": ["via"]}),
                    on_error: crate::services::OnError::Fail,
                    deadline_ms: 20_000,
                },
                crate::services::handler(|_p, _cx| async { Ok(json!({"via": ["core"]})) }),
            )
        })
        .expect("declare");
        for tag in ["one", "two"] {
            handle
                .eval(&format!(
                    r#"proc bridge_pt_{tag} {{}} {{
                        set req $::veles::service_request
                        set below [veles::service_next [veles::json_get $req next] {{}}]
                        if {{$::veles::service_request ne $req}} {{ error "request not restored" }}
                        return [string map [list "\]" ",\"{tag}\"\]"] $below]
                    }}"#
                ))
                .expect("proc");
        }
        crate::lifecycle::with_thread_owner("tcl:bridgetest-one", || {
            crate::services::register_tcl(
                decl("bridgetest.point"),
                "bridge_pt_one",
                tcl_service_handler("bridge_pt_one"),
            )
        })
        .expect("layer one");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let v = rt
            .block_on(crate::services::ask("bridgetest.point", json!({}), None))
            .expect("one over the Rust default");
        assert_eq!(v["via"], json!(["core", "one"]), "{v}");

        crate::lifecycle::with_thread_owner("tcl:bridgetest-two", || {
            crate::services::register_tcl(
                decl("bridgetest.point"),
                "bridge_pt_two",
                tcl_service_handler("bridge_pt_two"),
            )
        })
        .expect("layer two");
        let v = rt
            .block_on(crate::services::ask("bridgetest.point", json!({}), None))
            .expect("two over one, in place");
        assert_eq!(v["via"], json!(["core", "one", "two"]), "{v}");

        crate::services::purge_owner(Some("tcl:bridgetest-two"));
        crate::services::purge_owner(Some("tcl:bridgetest-one"));
        let v = rt
            .block_on(crate::services::ask("bridgetest.point", json!({}), None))
            .expect("the default again");
        assert_eq!(v["via"], json!(["core"]), "{v}");
        crate::services::purge_owner(Some("module:bridgetest-pt"));
    }

    #[test]
    fn send_outbound_without_a_gateway_fails_cleanly() {
        let (_home, _g, handle) = boot();
        let err = handle
            .eval("catch {veles::send_outbound telegram 123 hi} e; set e")
            .expect("catch");
        assert!(
            err.contains("gateway not running") || err.contains("no adapter"),
            "{err}"
        );
    }

    #[test]
    fn task_cancel_on_an_unknown_id_is_a_clean_error() {
        let (_home, _g, handle) = boot();
        // listing always answers, even empty
        let _ = handle.eval("veles::tasks").expect("tasks");
        let err = handle
            .eval("catch {veles::task_cancel 99999} e; set e")
            .expect("catch");
        assert!(err.contains("unknown or finished"), "{err}");
    }

    #[test]
    fn http_errors_are_clean_and_fast() {
        let (_home, _g, handle) = boot();
        // Port 1 on loopback refuses instantly — the point is the
        // failure shape (Tcl error text, no hang), not the network.
        let started = std::time::Instant::now();
        let err = handle
            .eval("catch {veles::http_get http://127.0.0.1:0/} e; set e")
            .expect("catch");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(err.to_lowercase().contains("http"), "{err}");
    }
    /// A one-shot HTTP server that echoes what it was sent. Used by the
    /// success-path tests below, which did not exist: `http_get` and
    /// `http_post` were only ever pointed at a closed port, so nothing
    /// checked that a 200 came back as a RESULT rather than an error,
    /// that the body arrived, or that a header argument did anything.
    async fn echo_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = std::sync::Arc::clone(&seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        crate::rt::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
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
                let req = String::from_utf8_lossy(&raw).to_string();
                cap.lock().unwrap().push(req.clone());
                let body = r#"{"echoed":true}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// The SUCCESS path for both HTTP ops, plus the headers argument.
    ///
    /// The bridge is reached from the Tcl thread, so the server has to
    /// run on a runtime of its own — hence the manual runtime rather
    /// than `#[tokio::test]`: `boot()` spawns a Tcl interpreter and
    /// `handle.eval` blocks, which `TclHandle::eval` refuses to do
    /// inside an async context.
    #[test]
    fn http_get_and_post_return_bodies_and_send_headers() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let (url, seen) = rt.block_on(echo_server());
        let (_home, _g, handle) = boot();

        let got = handle
            .eval(&format!("veles::http_get {url}/thing"))
            .expect("http_get");
        assert!(got.contains("echoed"), "the body must come back: {got}");

        let got = handle
            .eval(&format!(
                r#"veles::http_post {url}/thing {{{{"q":"hello"}}}} application/json"#
            ))
            .expect("http_post");
        assert!(got.contains("echoed"), "{got}");

        // Headers. Added for provider drivers, which cannot
        // authenticate without them — every non-bearer endpoint
        // (Anthropic's x-api-key, Azure's api-key) was unreachable.
        let got = handle
            .eval(&format!(
                r#"veles::http_post {url}/thing {{}} application/json {{{{"x-api-key":"sekrit","x-two":"2"}}}}"#
            ))
            .expect("http_post with headers");
        assert!(got.contains("echoed"), "{got}");

        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 3, "three calls reached the server");
        assert!(requests[0].starts_with("GET /thing"), "{}", requests[0]);
        assert!(requests[1].starts_with("POST /thing"), "{}", requests[1]);
        assert!(
            requests[1].contains(r#"{"q":"hello"}"#),
            "the POST body must arrive intact: {}",
            requests[1]
        );
        let third = requests[2].to_lowercase();
        assert!(third.contains("x-api-key: sekrit"), "{}", requests[2]);
        assert!(third.contains("x-two: 2"), "{}", requests[2]);

        // A malformed headers argument is refused by name, not ignored.
        let err = handle
            .eval(&format!(
                r#"catch {{veles::http_get {url}/thing {{not json}}}} e; set e"#
            ))
            .expect("catch");
        assert!(err.contains("headers"), "{err}");
    }

    /// `veles::call_llm` on the SUCCESS path.
    ///
    /// The command the whole "a Tcl script could drive the engine"
    /// claim rests on had never been executed in a test: every
    /// reference to it was inside a `catch` asserting it fails with no
    /// routes configured. So nothing checked that an answer comes back,
    /// that `provider` pins a route, or that an unknown provider is
    /// named rather than silently falling through to the default.
    #[test]
    fn call_llm_returns_the_assistant_text() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        // An OpenAI-shaped chat-completions answer.
        let (url, _seen) = rt.block_on(echo_completion_server());
        let (home, _g, handle) = boot_with_config(&format!(
            "[model]\nprovider = \"probe\"\ndefault = \"m1\"\n\n\
             [[custom_providers]]\nname = \"probe\"\nbase_url = \"{url}\"\n\
             key_env = \"VELES_PROBE_KEY\"\n"
        ));
        let _ = home;

        let out = handle
            .eval("veles::call_llm {you are terse} {say hi}")
            .expect("call_llm");
        assert_eq!(out, "the answer");

        // Pinning a provider by name works…
        let out = handle
            .eval("veles::call_llm {sys} {user} probe")
            .expect("pinned");
        assert_eq!(out, "the answer");

        // …and an unknown one is NAMED, not quietly ignored.
        let err = handle
            .eval("catch {veles::call_llm {s} {u} nosuchprovider} e; set e")
            .expect("catch");
        assert!(err.contains("nosuchprovider"), "{err}");
    }

    /// A chat-completions endpoint that always answers "the answer".
    async fn echo_completion_server() -> (String, ()) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        crate::rt::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"choices":[{"message":{"role":"assistant","content":"the answer"}}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), ())
    }

    /// `boot()` with a config of your own.
    fn boot_with_config(
        toml: &str,
    ) -> (
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        crate::tclrt::TclHandle,
    ) {
        let (home, guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), toml).expect("config");
        std::env::set_var("VELES_PROBE_KEY", "k");
        let handle = crate::tclrt::spawn(home.path().to_path_buf()).expect("tcl spawn");
        (home, guard, handle)
    }

    /// `veles::tasks` was evaluated into `let _ =` and asserted on
    /// nothing — a smoke test that could not fail. It returns the
    /// `/tasks` listing, which on an idle engine is a specific string
    /// and not an empty one.
    #[test]
    fn tasks_returns_the_listing() {
        let (_home, _g, handle) = boot();
        let out = handle.eval("veles::tasks").expect("tasks");
        assert_eq!(
            out,
            crate::tasks::listing(),
            "the op must return the same listing /tasks renders"
        );
        assert!(!out.is_empty(), "an idle listing still says something");
    }

    /// The hook off-list: four commands with no test at all, and the
    /// one property that actually matters about them — that switching a
    /// handler off SURVIVES a plugin reload.
    ///
    /// Without that, saving any plugin file silently switches every
    /// disabled handler back on, because sourcing a plugin re-runs its
    /// `::veles::hook` calls. `load_plugins` re-applies the persisted
    /// list on every load for exactly this reason and nothing checked it.
    #[test]
    fn a_disabled_hook_stays_disabled_across_a_reload() {
        let (home, _g, handle) = boot();
        // Two handlers on one hook; the first returns nothing, so the
        // second decides — which also pins dispatch ORDER.
        handle
            .eval(
                "proc h_quiet {} { return \"\" }\n\
                 proc h_loud {} { return \"LOUD\" }\n\
                 ::veles::hook post_llm_call h_quiet\n\
                 ::veles::hook post_llm_call h_loud",
            )
            .expect("register");
        assert_eq!(
            handle
                .eval("::veles::dispatch_hook post_llm_call")
                .expect("dispatch"),
            "LOUD",
            "first non-empty verdict wins, in registration order"
        );

        // Off, and the registry says so.
        handle
            .eval("::veles::hook_disable post_llm_call h_loud")
            .expect("disable");
        assert_eq!(
            handle
                .eval("::veles::hook_is_off post_llm_call h_loud")
                .expect("is_off"),
            "1"
        );
        assert_eq!(
            handle
                .eval("::veles::dispatch_hook post_llm_call")
                .expect("dispatch"),
            "",
            "a switched-off handler must be skipped"
        );
        assert!(
            handle
                .eval("::veles::hooks_list")
                .expect("list")
                .contains("(off)"),
            "the manager needs the row to still be visible"
        );

        // Persist it the way config does, then RELOAD.
        std::fs::write(
            home.path().join("config.toml"),
            "[hooks]\ndisabled = [\"post_llm_call h_loud\"]\n",
        )
        .expect("config");
        {
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle
                .send(crate::tclrt::TclRequest::Reload { reply: tx })
                .expect("send reload");
            rx.blocking_recv()
                .expect("reload replied")
                .expect("reload ok");
        }
        // The reload re-sources plugins, which re-registers handlers.
        handle
            .eval(
                "proc h_loud {} { return \"LOUD\" }\n\
                 ::veles::hook post_llm_call h_loud",
            )
            .expect("re-register");
        assert_eq!(
            handle
                .eval("::veles::hook_is_off post_llm_call h_loud")
                .expect("is_off"),
            "1",
            "the off-list must survive a reload, or saving any plugin file \
             silently switches every disabled handler back on"
        );

        // And back on, explicitly.
        handle
            .eval("::veles::hook_enable post_llm_call h_loud")
            .expect("enable");
        assert_eq!(
            handle
                .eval("::veles::hook_is_off post_llm_call h_loud")
                .expect("is_off"),
            "0"
        );
        handle.eval("::veles::hooks_off_clear").expect("clear");
        assert_eq!(
            handle
                .eval("::veles::hook_is_off post_llm_call h_loud")
                .expect("is_off"),
            "0"
        );
    }
}
