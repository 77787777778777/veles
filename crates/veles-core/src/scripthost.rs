//! The script tier's seam (PLAN-plugins-installer.md R9): what every
//! language host shares, so a host is only an engine.
//!
//! A language host (JavaScript, Racket, Common Lisp) is a MODULE: a dylib
//! on the shared runtime that embeds one engine and registers itself
//! here as the host of one manifest `kind` (`kind = "js"` in a plugin's
//! `plugin.toml`). This seam then does for its scripts what `tclrt` does
//! for Tcl plugins:
//!
//! - **discovery and lifecycle** — every enabled plugin of the host's kind
//!   is loaded when the host registers, re-loaded when the plugins
//!   directory changes (the Tcl watcher's trigger), and driven by the
//!   same verbs (`veles plugins unload|start|reload <name>`, `/plugins
//!   …`, the API) as the other tiers;
//! - **ownership** — each script is its own owner (`js:<name>`,
//!   `racket:<name>`, `lisp:<name>`), the tag a Tcl plugin gets
//!   (`tcl:<name>`): what it registers carries that owner, so an unload
//!   or a reload purges exactly its footprint through
//!   `modulehost::purge_owner_everywhere`, and nothing else's;
//! - **the engine API** — one dispatcher, [`ScriptCx::call`], that a host
//!   binds as ONE native function (`veles.call(op, …args)` in JS). The
//!   capability ops are `bridgeops::dispatch`'s table, the same the Tcl
//!   and ABI 4 tiers drive, so parity is structural; the four that take a
//!   callback (a tool, a command, a hook, a log line) are here, because a
//!   callback is an id the host resolves, not a proc name;
//! - **callbacks** — the tool, command and hook objects core holds for a
//!   script are late-bound shells that ask the host to `invoke` the
//!   script's callback by id, with a deadline, on whatever thread the
//!   engine runs. A script that is gone answers an error, never a hang.
//!
//! What a host does NOT do: touch a registry (it has no owner of its
//! own for a script's entries), read config or discover files (the seam
//! hands it a [`ScriptRef`]), or outlive its module (a purge of the
//! module's owner drops the host and its scripts' registrations in one
//! step, [`purge_owner_hosts`]).
//!
//! The engines are process-global (one QuickJS runtime, one Racket, one
//! ECL per process), so a HOST module stays mapped for the gateway's
//! life and the module host reports it `kept-mapped`; what reloads is
//! the script, inside the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::owned::OwnedRegistry;

/// The manifest kinds this seam serves. A plugin of any other kind is
/// another tier's (`tcl`, `rust`, `module`, `process`).
pub const SCRIPT_KINDS: &[&str] = &["js", "racket", "lisp"];

/// The source-file extensions the hosted kinds are written in. The
/// plugin watcher (`tclrt::start_plugin_watcher`) treats an edit to one
/// of these exactly as an edit to a `.tcl`: a reload trigger, and part
/// of the change digest. Before this list existed a `.ts` edit raised
/// an event the watcher dropped, and the digest would not have moved
/// either, so a script never hot-reloaded (scratch gateway, 2026-10-10).
/// A new host adds its extensions here, in the same commit as its kind.
pub const SCRIPT_SOURCE_EXTS: &[&str] = &["js", "mjs", "ts", "rkt", "lisp"];

/// The kinds whose engine has been booted in THIS process. Racket CS and
/// ECL are process-global: they boot once and cannot be booted again,
/// and their host module stays mapped for the gateway's life (its engine
/// thread keeps it). The module tier can still map a SECOND copy of such
/// a host — a `reload` after its file changed loads the new path beside
/// the kept-mapped old one — and that copy's `start` must refuse rather
/// than boot the engine a second time into the same process. The mark
/// lives here, in core, which no reload unmaps; the host's own statics
/// would be fresh in the new copy and know nothing.
static BOOTED_ENGINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Take the boot mark for `kind`: `true` for the first caller (boot the
/// engine), `false` when another copy already did (refuse).
pub fn mark_engine_booted(kind: &str) -> bool {
    let mut v = BOOTED_ENGINES.lock().unwrap_or_else(|p| p.into_inner());
    if v.iter().any(|k| k == kind) {
        return false;
    }
    v.push(kind.to_string());
    true
}

/// Has `kind`'s engine been booted in this process?
pub fn engine_booted(kind: &str) -> bool {
    BOOTED_ENGINES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|k| k == kind)
}

/// How long a script's tool may run: the bridge's own `tool_call`
/// deadline, so a tool a script offers and a tool a script calls are
/// held to the same clock.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// A slash command answers a person waiting at a prompt.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// A hook sits on the turn's path: a slow one stalls every turn.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
/// A service a script provides answers another plugin waiting on it.
pub const SERVICE_TIMEOUT: Duration = Duration::from_secs(60);
/// Loading a script: parse, evaluate, let it register.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A language host: one per kind, registered from its module's instance
/// thread (so the registry entry carries the module's owner).
pub trait ScriptHost: Send + Sync {
    /// The manifest kind this host serves (`"js"`).
    fn kind(&self) -> &'static str;
    /// Evaluate `script`'s entry. Everything it registers while loading
    /// goes through `script.cx`, which attributes it to the script's
    /// owner; the seam has purged an earlier load before calling this.
    /// Answers the host's line (what it compiled, how long it took).
    fn load(&self, script: ScriptRef) -> BoxFut<'_, Result<String, String>>;
    /// Drop the script's engine state (its context, its callbacks). The
    /// seam purges its registrations; the host drops only what it holds.
    fn unload(&self, name: &str) -> BoxFut<'_, Result<String, String>>;
    /// Run callback `callback` of script `name` with `payload` (a JSON
    /// text) and answer its result as JSON text. A script that is not
    /// loaded answers `Err`.
    fn invoke(
        &self,
        name: &str,
        callback: u64,
        payload: String,
    ) -> BoxFut<'_, Result<String, String>>;
}

/// One script, as the seam hands it to its host.
#[derive(Clone)]
pub struct ScriptRef {
    pub name: String,
    pub kind: &'static str,
    /// The plugin directory (`~/.config/veles/plugins/<name>/`).
    pub dir: PathBuf,
    /// The manifest's entry, resolved under `dir`.
    pub entry: PathBuf,
    /// `<kind>:<name>`, interned.
    pub owner: &'static str,
    /// The script's handle on the engine: the host binds `cx.call`.
    pub cx: Arc<ScriptCx>,
}

/// A script's identity and its one door into the engine.
pub struct ScriptCx {
    pub kind: &'static str,
    pub name: String,
    pub owner: &'static str,
    host: Arc<dyn ScriptHost>,
}

impl ScriptCx {
    /// The engine API, one op at a time. The ops are
    /// `bridgeops::OP_NAMES` (`tool_call`, `call_llm`, `http_get`,
    /// `config_get`, `note`, `send_outbound`, …: the same table the Tcl
    /// `veles::*` commands and the ABI 4 `host_call` drive), plus the
    /// four a host cannot express as a name across its boundary:
    ///
    /// - `register_tool <name> <description> <schema_json> <callback>`
    /// - `register_command <name> <description> <callback>`
    /// - `register_service <name> <description> <cap> <callback>`
    /// - `hook <hook_name> <callback>`
    /// - `log <level> <message>`
    ///
    /// A `callback` is a number the host resolves in `invoke`. Each
    /// registration carries the script's owner whatever thread calls.
    /// `service_call` is the table's op, answered here so the provider's
    /// gate and the announcement name THIS script as the caller.
    pub async fn call(&self, op: &str, args: Vec<String>) -> Result<String, String> {
        match op {
            "register_tool" => self.register_tool(&args),
            "register_command" => self.register_command(&args),
            "register_service" => self.register_service(&args),
            "service_call" => self.service_call(args).await,
            "hook" => self.hook(&args),
            "log" => {
                let (level, msg) = match args.as_slice() {
                    [level, msg] => (level.as_str(), msg.as_str()),
                    [msg] => ("info", msg.as_str()),
                    _ => return Err("usage: log ?level? <message>".into()),
                };
                let level = match level {
                    "error" => tracing::Level::ERROR,
                    "warn" | "warning" => tracing::Level::WARN,
                    "debug" => tracing::Level::DEBUG,
                    "trace" => tracing::Level::TRACE,
                    _ => tracing::Level::INFO,
                };
                crate::lifecycle::with_thread_owner(self.owner, || {
                    crate::module::log(level, msg);
                });
                Ok(String::new())
            }
            _ => {
                if !crate::bridgeops::OP_NAMES.contains(&op) {
                    return Err(format!(
                        "unknown engine op `{op}` (the ops: register_tool, register_command, \
                         register_service, hook, log, {})",
                        crate::bridgeops::OP_NAMES.join(", ")
                    ));
                }
                let bridge = crate::bridge::global().ok_or_else(|| {
                    "the engine bridge is not available (config unreadable)".to_string()
                })?;
                let op = op.to_string();
                // The bridge ops block on the bridge's own runtime: a
                // blocking thread, never an executor's.
                crate::rt::spawn_blocking(move || crate::bridgeops::dispatch(&bridge, &op, &args))
                    .await
                    .map_err(|e| format!("engine op panicked or was cancelled: {e}"))?
            }
        }
    }

    fn register_tool(&self, args: &[String]) -> Result<String, String> {
        let [name, description, schema, callback] = args else {
            return Err(
                "usage: register_tool <name> <description> <schema_json> <callback>".into(),
            );
        };
        let schema: Value = crate::bridge::parse_json_lenient(schema)
            .map_err(|e| format!("register_tool {name}: bad schema json: {e}"))?;
        let callback = parse_callback(callback)?;
        let tool = Arc::new(ScriptTool {
            cx: self.shared(),
            name: name.clone(),
            description: description.clone(),
            schema,
            callback,
            session_id: String::new(),
        });
        crate::lifecycle::with_thread_owner(self.owner, || {
            crate::tools::register_global_tool(tool);
        });
        Ok(format!("tool {name} registered"))
    }

    fn register_command(&self, args: &[String]) -> Result<String, String> {
        let [name, description, callback] = args else {
            return Err("usage: register_command <name> <description> <callback>".into());
        };
        let callback = parse_callback(callback)?;
        let cx = self.shared();
        let body: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(move |text: &str| {
            let payload = serde_json::json!({ "args": text }).to_string();
            match block_invoke(&cx, callback, payload, COMMAND_TIMEOUT) {
                Ok(out) => out,
                Err(e) => format!("error: {e}"),
            }
        });
        let name_owned = name.clone();
        crate::lifecycle::with_thread_owner(self.owner, || {
            crate::commands::register_command(crate::commands::PluginCommand {
                name: name_owned,
                description: description.clone(),
                body: crate::commands::CommandBody::Native(body),
            })
        })?;
        Ok(format!("command /{name} registered"))
    }

    /// Provide a service other plugins call (`crate::services`): the
    /// callback receives `{params, caller, session_id}` and answers the
    /// service's JSON. `cap` is who may call (empty = core and in-process
    /// modules only, `any`, or an ACL capability).
    fn register_service(&self, args: &[String]) -> Result<String, String> {
        let [name, description, cap, callback] = args else {
            return Err("usage: register_service <name> <description> <cap> <callback>".into());
        };
        let callback = parse_callback(callback)?;
        let decl = crate::services::ServiceDecl {
            name: name.clone(),
            description: description.clone(),
            cap: Some(cap.clone()).filter(|c| !c.trim().is_empty()),
        };
        let cx = self.shared();
        let handler = crate::services::handler(move |params, call_cx| {
            let cx = Arc::clone(&cx);
            async move {
                let payload = crate::services::script_payload(&params, &call_cx).to_string();
                let out = tokio::time::timeout(
                    SERVICE_TIMEOUT,
                    cx.host.invoke(&cx.name, callback, payload),
                )
                .await
                .map_err(|_| {
                    format!(
                        "{}:{}: service callback {callback} did not answer within {}s",
                        cx.kind,
                        cx.name,
                        SERVICE_TIMEOUT.as_secs()
                    )
                })??;
                // The host answers JSON text (`null` for nothing).
                Ok(serde_json::from_str(&out).unwrap_or(Value::String(out)))
            }
        });
        crate::lifecycle::with_thread_owner(self.owner, || {
            crate::services::register(decl, handler)
        })?;
        Ok(format!("service {name} registered"))
    }

    /// `service_call <name> <params_json> ?session_id?`: the table's op,
    /// with this script named as the caller.
    async fn service_call(&self, args: Vec<String>) -> Result<String, String> {
        let (name, params, sid) = match args.as_slice() {
            [n, p] => (n.clone(), p.clone(), None),
            [n, p, s] => (n.clone(), p.clone(), Some(s.clone())),
            _ => return Err("usage: service_call <name> <params_json> ?session_id?".into()),
        };
        let params: Value = crate::bridge::parse_json_lenient(&params)
            .map_err(|e| format!("bad params json: {e}"))?;
        let out = crate::services::call_from_plugin(self.owner, &name, params, sid).await?;
        Ok(match out {
            Value::String(s) => s,
            v => v.to_string(),
        })
    }

    fn hook(&self, args: &[String]) -> Result<String, String> {
        let [hook, callback] = args else {
            return Err("usage: hook <hook_name> <callback>".into());
        };
        if !crate::hooks::KNOWN_HOOKS.contains(&hook.as_str()) {
            return Err(format!(
                "unknown hook `{hook}` (the hooks: {})",
                crate::hooks::KNOWN_HOOKS.join(", ")
            ));
        }
        let callback = parse_callback(callback)?;
        let hooks = {
            let mut table = hooks_table().lock().unwrap_or_else(|p| p.into_inner());
            Arc::clone(table.entry(self.name.clone()).or_insert_with(|| {
                Arc::new(ScriptHooks {
                    cx: self.shared(),
                    hooks: Mutex::new(Vec::new()),
                })
            }))
        };
        {
            let mut list = hooks.hooks.lock().unwrap_or_else(|p| p.into_inner());
            list.retain(|(h, _)| h != hook);
            list.push((hook.clone(), callback));
        }
        // Replace-by-name: the plugin object is re-registered with its
        // grown list, under the script's owner.
        let plugin: Arc<dyn crate::rustplugins::VelesPlugin> = hooks;
        crate::lifecycle::with_thread_owner(self.owner, || {
            crate::rustplugins::register_plugin(plugin);
        });
        Ok(format!("hook {hook} registered"))
    }

    /// The same identity as an `Arc`, for the shells that outlive this
    /// borrow (a tool clone bound to a session).
    fn shared(&self) -> Arc<ScriptCx> {
        Arc::new(ScriptCx {
            kind: self.kind,
            name: self.name.clone(),
            owner: self.owner,
            host: Arc::clone(&self.host),
        })
    }
}

fn parse_callback(s: &str) -> Result<u64, String> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| format!("callback must be the id the host handed out, not `{s}`"))
}

/// Ask the host for a callback's result and turn it into the string a
/// tool, command or hook answers: a JSON string is its text, `null` or
/// an empty answer is "", anything else is its JSON.
async fn invoke(
    cx: &ScriptCx,
    callback: u64,
    payload: String,
    within: Duration,
) -> Result<String, String> {
    match tokio::time::timeout(within, cx.host.invoke(&cx.name, callback, payload)).await {
        Ok(Ok(out)) => Ok(decode_result(&out)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "{}:{}: callback {callback} did not answer within {}s",
            cx.kind,
            cx.name,
            within.as_secs()
        )),
    }
}

/// `invoke` from a SYNC surface (a native command body, a hook
/// handler): both run where blocking is allowed by contract, on a
/// blocking thread. Blocks on this seam's own small runtime, so no
/// executor thread is ever parked and a script's callback can itself
/// call back into the engine.
fn block_invoke(
    cx: &Arc<ScriptCx>,
    callback: u64,
    payload: String,
    within: Duration,
) -> Result<String, String> {
    let cx = Arc::clone(cx);
    let fut = async move { invoke(&cx, callback, payload, within).await };
    match tokio::runtime::Handle::try_current() {
        // Called with a multi-thread runtime entered (a blocking-pool
        // thread, or a caller that fires handlers inline from a task):
        // tell that runtime this thread blocks now, so a nested
        // `block_on` is allowed instead of a panic.
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| callback_rt().block_on(fut))
        }
        _ => callback_rt().block_on(fut),
    }
}

fn callback_rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("veles-script-callbacks")
            .enable_all()
            .build()
            .expect("the script callback runtime builds")
    })
}

/// A host's answer, as the engine-facing string. Pure, so the rule is
/// testable: `"text"` → text, `null`/`""` → "", `{…}` → its JSON text,
/// a non-JSON answer → itself.
pub fn decode_result(out: &str) -> String {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::String(s)) => s,
        Ok(Value::Null) => String::new(),
        Ok(other) => other.to_string(),
        Err(_) => out.to_string(),
    }
}

// ── the shells core holds for a script ───────────────────────────────

struct ScriptTool {
    cx: Arc<ScriptCx>,
    name: String,
    description: String,
    schema: Value,
    callback: u64,
    session_id: String,
}

#[async_trait::async_trait]
impl crate::tools::Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    async fn call(&self, args: Value) -> String {
        let payload =
            serde_json::json!({ "args": args, "session_id": self.session_id }).to_string();
        match invoke(&self.cx, self.callback, payload, TOOL_TIMEOUT).await {
            Ok(out) => out,
            Err(e) => format!("error: tool `{}` failed: {e}", self.name),
        }
    }
    /// Bound to the calling session, so the script sees whose call it
    /// is (and can name it to `tool_call`).
    fn for_session(&self, session_id: &str) -> Option<Arc<dyn crate::tools::Tool>> {
        Some(Arc::new(ScriptTool {
            cx: Arc::clone(&self.cx),
            name: self.name.clone(),
            description: self.description.clone(),
            schema: self.schema.clone(),
            callback: self.callback,
            session_id: session_id.to_string(),
        }))
    }
}

/// The hook handlers a script registered, as the one rust-plugin object
/// core fires (`rustplugins::run_hooks` walks `hooks()` per fire, on a
/// blocking thread).
struct ScriptHooks {
    cx: Arc<ScriptCx>,
    hooks: Mutex<Vec<(String, u64)>>,
}

impl crate::rustplugins::VelesPlugin for ScriptHooks {
    fn name(&self) -> &str {
        self.cx.owner
    }
    fn version(&self) -> &str {
        "script"
    }
    fn hooks(&self) -> Vec<(String, crate::rustplugins::RustHookFn)> {
        let list = self.hooks.lock().unwrap_or_else(|p| p.into_inner()).clone();
        list.into_iter()
            .map(|(hook, callback)| {
                let cx = Arc::clone(&self.cx);
                let hook_name = hook.clone();
                let f: crate::rustplugins::RustHookFn = Arc::new(move |args_json: &str| {
                    let args: Value = serde_json::from_str(args_json)
                        .unwrap_or(Value::String(args_json.to_string()));
                    let payload =
                        serde_json::json!({ "hook": hook_name, "args": args }).to_string();
                    match block_invoke(&cx, callback, payload, HOOK_TIMEOUT) {
                        Ok(v) if !v.is_empty() => Some(v),
                        Ok(_) => None,
                        Err(e) => {
                            tracing::warn!("{}: hook {hook_name} failed: {e}", cx.owner);
                            None
                        }
                    }
                });
                (hook, f)
            })
            .collect()
    }
}

fn hooks_table() -> &'static Mutex<BTreeMap<String, Arc<ScriptHooks>>> {
    static T: OnceLock<Mutex<BTreeMap<String, Arc<ScriptHooks>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(BTreeMap::new()))
}

// ── the host registry and the script table ───────────────────────────

fn hosts() -> &'static OwnedRegistry<String, Arc<dyn ScriptHost>> {
    static H: OnceLock<OwnedRegistry<String, Arc<dyn ScriptHost>>> = OnceLock::new();
    H.get_or_init(OwnedRegistry::default)
}

/// Register `host` as the host of its kind, under the calling thread's
/// owner (a module's instance thread, M4). A second host of the same
/// kind replaces the first. The host then loads its scripts with
/// [`load_all_for_kind`]; nothing is loaded here.
pub fn register_host(host: Arc<dyn ScriptHost>) {
    let kind = host.kind().to_string();
    hosts().insert(kind, host);
}

/// The host of `kind`, if one is registered.
pub fn host_for(kind: &str) -> Option<Arc<dyn ScriptHost>> {
    hosts().with(&kind.to_string(), Arc::clone)
}

/// The kinds with a host right now.
pub fn hosted_kinds() -> Vec<String> {
    hosts().keys()
}

/// Drop every host `owner` registered AND every script those hosts
/// served: the scripts' registrations are purged by their own owners,
/// their rows cleared. Part of `purge_owner_everywhere`, so a host
/// module's unload or reload leaves nothing of its scripts behind.
/// Returns the entries removed (hosts + scripts).
pub fn purge_owner_hosts(owner: Option<&'static str>) -> usize {
    let Some(owner) = owner else { return 0 };
    let mut kinds = Vec::new();
    hosts().for_each(|k, entry| {
        if entry.owner() == Some(owner) {
            kinds.push(k.clone());
        }
    });
    let mut n = hosts().purge(Some(owner));
    if kinds.is_empty() {
        return n;
    }
    let gone: Vec<(String, &'static str)> = {
        let mut table = scripts().lock().unwrap_or_else(|p| p.into_inner());
        let names: Vec<String> = table
            .iter()
            .filter(|(_, s)| kinds.iter().any(|k| k == s.kind))
            .map(|(name, _)| name.clone())
            .collect();
        names
            .into_iter()
            .filter_map(|name| table.remove(&name).map(|s| (name, s.owner)))
            .collect()
    };
    for (name, script_owner) in gone {
        hooks_table()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&name);
        let purged = crate::modulehost::purge_owner_everywhere(Some(script_owner));
        tracing::info!("{script_owner}: its host went away ({purged} registration(s) purged)");
        n += 1;
    }
    n
}

/// `<kind>:<name>`, interned once per distinct tag.
pub fn owner_tag_for(kind: &str, name: &str) -> &'static str {
    crate::lifecycle::owner_tag(&format!("{kind}:{name}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Loaded,
    Error,
}

#[derive(Clone, Debug)]
struct ScriptState {
    kind: &'static str,
    owner: &'static str,
    state: State,
    detail: String,
}

fn scripts() -> &'static Mutex<BTreeMap<String, ScriptState>> {
    static S: OnceLock<Mutex<BTreeMap<String, ScriptState>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Scripts an `unload` verb took out: they stay out of every load,
/// the watcher's included, until `start` names them (tclrt's rule).
fn unloaded() -> &'static Mutex<BTreeSet<String>> {
    static U: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    U.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub fn is_unloaded(name: &str) -> bool {
    unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(name)
}

/// Load one script: purge what an earlier load registered, hand the
/// host a [`ScriptRef`], record the outcome. A failed load leaves
/// nothing registered either (the partial registrations are purged
/// again) and the row says `error` with the message.
pub async fn load_script(
    host: Arc<dyn ScriptHost>,
    p: &crate::plugins::LoadedPlugin,
) -> Result<String, String> {
    let kind = host.kind();
    let name = p.manifest.name.clone();
    let owner = owner_tag_for(kind, &name);
    let entry = p.dir.join(&p.manifest.entry);
    let purged = forget(&name, owner);
    crate::plugins::declare_services(p);
    if !entry.exists() {
        let msg = format!("{owner}: entry {} is missing", entry.display());
        record(&name, kind, owner, State::Error, msg.clone());
        return Err(msg);
    }
    let cx = Arc::new(ScriptCx {
        kind,
        name: name.clone(),
        owner,
        host: Arc::clone(&host),
    });
    let script = ScriptRef {
        name: name.clone(),
        kind,
        dir: p.dir.clone(),
        entry,
        owner,
        cx,
    };
    let t0 = std::time::Instant::now();
    let r = match tokio::time::timeout(LOAD_TIMEOUT, host.load(script)).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "did not finish loading within {}s",
            LOAD_TIMEOUT.as_secs()
        )),
    };
    match r {
        Ok(line) => {
            let msg = format!(
                "{owner}: loaded in {} ms{} — {line}",
                t0.elapsed().as_millis(),
                if purged > 0 {
                    format!(" ({purged} registration(s) of its earlier load purged first)")
                } else {
                    String::new()
                }
            );
            record(&name, kind, owner, State::Loaded, line);
            tracing::info!("{msg}");
            Ok(msg)
        }
        Err(e) => {
            let _ = crate::modulehost::purge_owner_everywhere(Some(owner));
            hooks_table()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&name);
            let msg = format!("{owner}: load failed: {e}");
            record(&name, kind, owner, State::Error, e);
            tracing::warn!("{msg}");
            Err(msg)
        }
    }
}

/// Drop a script's registrations and its hook object; answers how many
/// registrations went.
fn forget(name: &str, owner: &'static str) -> usize {
    hooks_table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(name);
    crate::modulehost::purge_owner_everywhere(Some(owner))
}

fn record(name: &str, kind: &'static str, owner: &'static str, state: State, detail: String) {
    scripts().lock().unwrap_or_else(|p| p.into_inner()).insert(
        name.to_string(),
        ScriptState {
            kind,
            owner,
            state,
            detail,
        },
    );
}

/// Unload one script: the host drops its state, the seam its
/// registrations; it stays out until `start`.
pub async fn unload_script(name: &str) -> Result<String, String> {
    let (kind, owner) = {
        let table = scripts().lock().unwrap_or_else(|p| p.into_inner());
        match table.get(name) {
            Some(s) => (s.kind, s.owner),
            None => return Err(format!("no loaded script named `{name}`")),
        }
    };
    let host_line = match host_for(kind) {
        Some(host) => host
            .unload(name)
            .await
            .unwrap_or_else(|e| format!("host: {e}")),
        None => "no host".to_string(),
    };
    let purged = forget(name, owner);
    scripts()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(name);
    unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(name.to_string());
    let msg = format!(
        "{owner}: unloaded ({purged} registration(s) purged; {host_line}); `start` loads it again"
    );
    tracing::info!("{msg}");
    Ok(msg)
}

/// Load every enabled plugin of `kind` that is not held out by an
/// `unload` (a host's start, the watcher's reload). Answers one line
/// per script.
pub async fn load_all_for_kind(kind: &str) -> Vec<String> {
    let cfg = crate::config::load().unwrap_or_default();
    let discovered =
        crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled).unwrap_or_default();
    load_all_in(kind, &discovered).await
}

/// [`load_all_for_kind`] over an explicit discovery (what the tests
/// drive from a scratch directory).
pub async fn load_all_in(kind: &str, discovered: &[crate::plugins::LoadedPlugin]) -> Vec<String> {
    let Some(host) = host_for(kind) else {
        return vec![format!("no {kind} host is loaded")];
    };
    let mut out = Vec::new();
    for p in discovered.iter().filter(|p| p.manifest.kind == kind) {
        if is_unloaded(&p.manifest.name) {
            out.push(format!("{kind}:{}: unloaded; not loaded", p.manifest.name));
            continue;
        }
        out.push(
            load_script(Arc::clone(&host), p)
                .await
                .unwrap_or_else(|e| e),
        );
    }
    out
}

/// Re-load every hosted kind's scripts: the plugins directory changed.
pub async fn reload_all() -> Vec<String> {
    let mut out = Vec::new();
    for kind in hosted_kinds() {
        out.extend(load_all_for_kind(&kind).await);
    }
    out
}

/// [`reload_all`] from a thread that is not an executor's (the plugin
/// watcher's std thread): runs it on the detached background runtime
/// and waits. A no-op when no host is registered.
pub fn reload_all_blocking() -> Vec<String> {
    if hosted_kinds().is_empty() {
        return Vec::new();
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::bg::handle().spawn(async move {
        let _ = tx.send(reload_all().await);
    });
    rx.blocking_recv().unwrap_or_default()
}

/// A verb for `target` when it names a plugin of a script kind:
/// `unload`/`stop` take it out, `start`/`reload` load it (clearing a
/// hold-out). `None` when no enabled plugin of a script kind has that
/// name, so the caller tries the next tier.
pub fn request(target: &str, verb: &str) -> Option<BoxFut<'static, Result<String, String>>> {
    let cfg = crate::config::load().unwrap_or_default();
    let discovered =
        crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled).unwrap_or_default();
    request_in(target, verb, discovered)
}

/// [`request`] over an explicit discovery.
pub fn request_in(
    target: &str,
    verb: &str,
    discovered: Vec<crate::plugins::LoadedPlugin>,
) -> Option<BoxFut<'static, Result<String, String>>> {
    let p = discovered
        .into_iter()
        .find(|p| p.manifest.name == target && SCRIPT_KINDS.contains(&p.manifest.kind.as_str()))?;
    let unload = matches!(verb, "unload" | "stop");
    Some(Box::pin(async move {
        let kind = p.manifest.kind.clone();
        let name = p.manifest.name.clone();
        if unload {
            return unload_script(&name).await;
        }
        let Some(host) = host_for(&kind) else {
            return Err(format!("no {kind} host is loaded (the lang-{kind} module)"));
        };
        unloaded()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&name);
        load_script(host, &p).await
    }))
}

/// One row per enabled plugin of a script kind, for the listings:
/// `loaded`, `error` (with the message), `unloaded` (held out),
/// `pending` (its host is here, the load not yet), `no-host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRow {
    pub name: String,
    pub kind: String,
    pub state: &'static str,
    pub detail: String,
}

pub fn rows() -> Vec<ScriptRow> {
    let cfg = crate::config::load().unwrap_or_default();
    let discovered =
        crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled).unwrap_or_default();
    rows_in(&discovered)
}

pub fn rows_in(discovered: &[crate::plugins::LoadedPlugin]) -> Vec<ScriptRow> {
    let table = scripts().lock().unwrap_or_else(|p| p.into_inner());
    let hosted = hosted_kinds();
    discovered
        .iter()
        .filter(|p| SCRIPT_KINDS.contains(&p.manifest.kind.as_str()))
        .map(|p| {
            let name = p.manifest.name.clone();
            let kind = p.manifest.kind.clone();
            let (state, detail) = if is_unloaded(&name) {
                ("unloaded", String::new())
            } else if let Some(s) = table.get(&name) {
                match s.state {
                    State::Loaded => ("loaded", String::new()),
                    State::Error => ("error", s.detail.clone()),
                }
            } else if hosted.contains(&kind) {
                ("pending", String::new())
            } else {
                ("no-host", format!("no {kind} host is loaded"))
            };
            ScriptRow {
                name,
                kind,
                state,
                detail,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{LoadedPlugin, PluginManifest};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A host that registers whatever its script's entry file asks for
    /// (one op per line: `register_tool …` etc.), and answers invokes
    /// with a recorded result.
    struct FakeHost {
        kind: &'static str,
        loads: AtomicUsize,
        unloads: AtomicUsize,
        invokes: Mutex<Vec<(String, u64, String)>>,
        answer: Mutex<String>,
        /// A hook payload without this word gets `null` (no opinion):
        /// the core test binary fires hooks from other tests too, and
        /// a fake that answered every fire would decide theirs.
        magic: &'static str,
    }

    impl FakeHost {
        fn new(kind: &'static str, magic: &'static str) -> Arc<Self> {
            Arc::new(FakeHost {
                kind,
                loads: AtomicUsize::new(0),
                unloads: AtomicUsize::new(0),
                invokes: Mutex::new(Vec::new()),
                answer: Mutex::new("\"ok\"".into()),
                magic,
            })
        }
    }

    impl ScriptHost for FakeHost {
        fn kind(&self) -> &'static str {
            self.kind
        }
        fn load(&self, script: ScriptRef) -> BoxFut<'_, Result<String, String>> {
            Box::pin(async move {
                self.loads.fetch_add(1, Ordering::SeqCst);
                let src = std::fs::read_to_string(&script.entry).map_err(|e| e.to_string())?;
                let mut n = 0;
                for line in src.lines().filter(|l| !l.trim().is_empty()) {
                    if line.trim() == "FAIL" {
                        return Err("the script threw".into());
                    }
                    let parts: Vec<String> =
                        line.split('|').map(|s| s.trim().to_string()).collect();
                    let (op, args) = parts.split_first().expect("an op");
                    script.cx.call(op, args.to_vec()).await?;
                    n += 1;
                }
                Ok(format!("{n} op(s)"))
            })
        }
        fn unload(&self, _name: &str) -> BoxFut<'_, Result<String, String>> {
            Box::pin(async move {
                self.unloads.fetch_add(1, Ordering::SeqCst);
                Ok("context dropped".into())
            })
        }
        fn invoke(
            &self,
            name: &str,
            callback: u64,
            payload: String,
        ) -> BoxFut<'_, Result<String, String>> {
            let name = name.to_string();
            Box::pin(async move {
                if payload.contains("\"hook\":") && !payload.contains(self.magic) {
                    return Ok("null".into());
                }
                self.invokes.lock().unwrap().push((name, callback, payload));
                Ok(self.answer.lock().unwrap().clone())
            })
        }
    }

    fn plugin(dir: &std::path::Path, name: &str, kind: &str, body: &str) -> LoadedPlugin {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.fake"), body).unwrap();
        LoadedPlugin {
            manifest: PluginManifest {
                name: name.into(),
                kind: kind.into(),
                provides_hooks: Vec::new(),
                entry: "plugin.fake".into(),
                description: String::new(),
                api: None,
            },
            dir: d,
        }
    }

    fn tool_owner(name: &str) -> Option<Option<&'static str>> {
        crate::tools::extra_tools().owner_of(&name.to_string())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_script_registers_under_its_own_owner_and_an_unload_purges_it() {
        let tmp = tempfile::tempdir().unwrap();
        let host = FakeHost::new("js", "t1-token");
        let host_dyn: Arc<dyn ScriptHost> = host.clone();
        crate::lifecycle::with_thread_owner("module:lang-js-t1", || register_host(host_dyn));
        let p = plugin(
            tmp.path(),
            "t1-demo",
            "js",
            "register_tool | t1_tool | a tool | {\"type\":\"object\"} | 7\nregister_command | t1cmd | a command | 8\nhook | post_llm_call | 9\n",
        );
        let lines = load_all_in("js", std::slice::from_ref(&p)).await;
        assert!(lines[0].contains("js:t1-demo: loaded"), "{lines:?}");
        assert_eq!(tool_owner("t1_tool"), Some(Some("js:t1-demo")));
        assert!(crate::commands::plugin_command("t1cmd").is_some());
        assert!(crate::rustplugins::has_rust_handler("post_llm_call"));
        let rows = rows_in(std::slice::from_ref(&p));
        assert_eq!(rows[0].state, "loaded", "{rows:?}");

        // The tool's call reaches the host with the args and the session.
        let tool = crate::tools::extra_tools()
            .with(&"t1_tool".to_string(), Arc::clone)
            .unwrap();
        let bound = tool.for_session("s-77").unwrap();
        let out = bound.call(serde_json::json!({"x": 1})).await;
        assert_eq!(out, "ok");
        let inv = host.invokes.lock().unwrap().last().cloned().unwrap();
        assert_eq!((inv.0.as_str(), inv.1), ("t1-demo", 7));
        assert!(inv.2.contains("\"session_id\":\"s-77\""), "{}", inv.2);

        // A reload purges the earlier load first: still exactly one tool.
        let lines = load_all_in("js", std::slice::from_ref(&p)).await;
        assert!(lines[0].contains("purged first"), "{lines:?}");
        assert_eq!(host.loads.load(Ordering::SeqCst), 2);

        // Unload: everything gone, held out of the next load until start.
        let line = request_in("t1-demo", "unload", vec![p.clone()])
            .unwrap()
            .await
            .unwrap();
        assert!(line.contains("unloaded"), "{line}");
        assert_eq!(host.unloads.load(Ordering::SeqCst), 1);
        assert_eq!(tool_owner("t1_tool"), None);
        assert!(crate::commands::plugin_command("t1cmd").is_none());
        assert_eq!(rows_in(std::slice::from_ref(&p))[0].state, "unloaded");
        let lines = load_all_in("js", std::slice::from_ref(&p)).await;
        assert!(lines[0].contains("unloaded; not loaded"), "{lines:?}");
        assert_eq!(host.loads.load(Ordering::SeqCst), 2);
        let line = request_in("t1-demo", "start", vec![p.clone()])
            .unwrap()
            .await
            .unwrap();
        assert!(line.contains("loaded"), "{line}");
        assert_eq!(tool_owner("t1_tool"), Some(Some("js:t1-demo")));
        // Leave nothing behind for the sibling tests.
        unload_script("t1-demo").await.unwrap();
        purge_owner_hosts(Some("module:lang-js-t1"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_load_leaves_nothing_registered_and_says_error() {
        let tmp = tempfile::tempdir().unwrap();
        let host = FakeHost::new("racket", "t2-token");
        let host_dyn: Arc<dyn ScriptHost> = host.clone();
        crate::lifecycle::with_thread_owner("module:lang-racket-t2", || register_host(host_dyn));
        let p = plugin(
            tmp.path(),
            "t2-broken",
            "racket",
            "register_tool | t2_tool | a tool | {} | 1\nFAIL\n",
        );
        let lines = load_all_in("racket", std::slice::from_ref(&p)).await;
        assert!(
            lines[0].contains("load failed: the script threw"),
            "{lines:?}"
        );
        assert_eq!(
            tool_owner("t2_tool"),
            None,
            "the partial registration is purged"
        );
        let rows = rows_in(std::slice::from_ref(&p));
        assert_eq!(
            (rows[0].state, rows[0].detail.as_str()),
            ("error", "the script threw")
        );
        purge_owner_hosts(Some("module:lang-racket-t2"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hosts_purge_takes_its_scripts_registrations_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let host = FakeHost::new("lisp", "t3-token");
        let host_dyn: Arc<dyn ScriptHost> = host.clone();
        crate::lifecycle::with_thread_owner("module:lang-lisp-t3", || register_host(host_dyn));
        let p = plugin(
            tmp.path(),
            "t3-demo",
            "lisp",
            "register_tool | t3_tool | a tool | {} | 1\n",
        );
        load_all_in("lisp", std::slice::from_ref(&p)).await;
        assert_eq!(tool_owner("t3_tool"), Some(Some("lisp:t3-demo")));
        assert!(hosted_kinds().contains(&"lisp".to_string()));
        let n = purge_owner_hosts(Some("module:lang-lisp-t3"));
        assert_eq!(n, 2, "the host and its one script");
        assert_eq!(tool_owner("t3_tool"), None);
        assert!(!hosted_kinds().contains(&"lisp".to_string()));
        assert_eq!(rows_in(std::slice::from_ref(&p))[0].state, "no-host");
        // Another owner's purge touches nothing.
        assert_eq!(purge_owner_hosts(Some("module:somebody-else")), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hook_and_a_command_answer_through_the_host_on_a_blocking_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let host = FakeHost::new("fake-t4", "t4-token");
        let host_dyn: Arc<dyn ScriptHost> = host.clone();
        crate::lifecycle::with_thread_owner("module:lang-js-t4", || register_host(host_dyn));
        let p = plugin(
            tmp.path(),
            "t4-hooks",
            "fake-t4",
            "hook | post_llm_call | 3\nregister_command | t4cmd | a command | 4\n",
        );
        load_all_in("fake-t4", std::slice::from_ref(&p)).await;
        *host.answer.lock().unwrap() = r#"{"decision":"from-script"}"#.into();
        // Hooks run on a blocking thread (HookBus::fire's contract).
        let verdict = crate::rt::spawn_blocking(|| {
            crate::rustplugins::run_hooks(
                "post_llm_call",
                r#"{"session_id":"s-1","text":"hi t4-token"}"#,
            )
        })
        .await
        .unwrap();
        assert_eq!(verdict.as_deref(), Some(r#"{"decision":"from-script"}"#));
        let inv = host.invokes.lock().unwrap().last().cloned().unwrap();
        assert_eq!(inv.1, 3);
        assert!(
            inv.2.contains("\"hook\":\"post_llm_call\"")
                && inv.2.contains("\"text\":\"hi t4-token\""),
            "{}",
            inv.2
        );
        // An empty answer is "no opinion".
        *host.answer.lock().unwrap() = "null".into();
        let verdict = crate::rt::spawn_blocking(|| {
            crate::rustplugins::run_hooks("post_llm_call", r#"{"text":"t4-token again"}"#)
        })
        .await
        .unwrap();
        assert_eq!(verdict, None);
        // The command renders through the same door.
        *host.answer.lock().unwrap() = "\"rendered\"".into();
        let cmd = crate::commands::plugin_command("t4cmd").unwrap();
        let out = crate::commands::render_plugin_async(&cmd, "some text")
            .await
            .unwrap();
        assert_eq!(out, "rendered");
        let inv = host.invokes.lock().unwrap().last().cloned().unwrap();
        assert_eq!((inv.1, inv.2.as_str()), (4, r#"{"args":"some text"}"#));
        unload_script("t4-hooks").await.unwrap();
        purge_owner_hosts(Some("module:lang-js-t4"));
    }

    /// The hook shell itself turns an empty decoded answer into "no
    /// opinion". Proven on the shell's own closure: `run_hooks` drops
    /// empty verdicts too, so through it this guard is shadowed (the
    /// 2026-10-10 mutant run found that out).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_answer_from_the_hook_shell_is_no_opinion() {
        let host = FakeHost::new("fake-t6", "t6-token");
        let host_dyn: Arc<dyn ScriptHost> = host.clone();
        let shell = ScriptHooks {
            cx: Arc::new(ScriptCx {
                kind: "fake-t6",
                name: "t6-shell".into(),
                owner: "js:t6-shell",
                host: host_dyn,
            }),
            hooks: Mutex::new(vec![("post_llm_call".to_string(), 6)]),
        };
        let (name, f) = crate::rustplugins::VelesPlugin::hooks(&shell)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(name, "post_llm_call");
        // "null" decodes to the empty text: no opinion.
        *host.answer.lock().unwrap() = "null".into();
        let g = Arc::clone(&f);
        let v = crate::rt::spawn_blocking(move || g(r#"{"text":"t6-token"}"#))
            .await
            .unwrap();
        assert_eq!(v, None);
        let inv = host.invokes.lock().unwrap().last().cloned().unwrap();
        assert_eq!(inv.1, 6, "the shell did call the host: {inv:?}");
        // A real answer comes back as the verdict.
        *host.answer.lock().unwrap() = r#"{"decision":"from-shell"}"#.into();
        let v = crate::rt::spawn_blocking(move || f(r#"{"text":"t6-token"}"#))
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some(r#"{"decision":"from-shell"}"#));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_script_api_refuses_what_it_does_not_know() {
        let host = FakeHost::new("js", "t5-token");
        let host_dyn: Arc<dyn ScriptHost> = host.clone();
        let cx = ScriptCx {
            kind: "js",
            name: "t5".into(),
            owner: owner_tag_for("js", "t5"),
            host: host_dyn,
        };
        let e = cx.call("frobnicate", vec![]).await.unwrap_err();
        assert!(e.contains("unknown engine op `frobnicate`"), "{e}");
        let e = cx
            .call("hook", vec!["no_such_hook".into(), "1".into()])
            .await
            .unwrap_err();
        assert!(e.contains("unknown hook `no_such_hook`"), "{e}");
        let e = cx
            .call(
                "register_tool",
                vec!["t".into(), "d".into(), "{}".into(), "x".into()],
            )
            .await
            .unwrap_err();
        assert!(e.contains("callback must be the id"), "{e}");
        let e = cx
            .call("register_tool", vec!["t".into()])
            .await
            .unwrap_err();
        assert!(e.starts_with("usage: register_tool"), "{e}");
        assert_eq!(
            cx.call("log", vec!["warn".into(), "a line".into()])
                .await
                .unwrap(),
            ""
        );
        assert_eq!(crate::modulehost::purge_owner_everywhere(Some(cx.owner)), 0);
    }

    /// A host whose engine is process-global (Racket CS, ECL) boots it
    /// once. The module tier can map a SECOND copy of such a host (a
    /// reload after the file changed: the first stays mapped by its
    /// engine thread), and that copy must be told the engine is already
    /// up instead of booting it again into the same process. The mark
    /// lives in core, which no reload unmaps.
    #[test]
    fn an_engine_boots_once_per_process_and_a_second_copy_is_refused() {
        assert!(!engine_booted("t-pin-kind"));
        assert!(
            mark_engine_booted("t-pin-kind"),
            "the first boot takes the mark"
        );
        assert!(engine_booted("t-pin-kind"));
        assert!(
            !mark_engine_booted("t-pin-kind"),
            "a second copy of the host must be told no"
        );
        assert!(!engine_booted("t-pin-other"), "the mark is per kind");
    }

    #[test]
    fn a_hosts_answer_decodes_to_the_text_a_tool_returns() {
        assert_eq!(decode_result("\"hello\""), "hello");
        assert_eq!(decode_result("null"), "");
        assert_eq!(decode_result(""), "");
        assert_eq!(decode_result("   "), "");
        assert_eq!(decode_result(r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(decode_result("42"), "42");
        assert_eq!(decode_result("not json"), "not json");
    }

    #[test]
    fn a_verb_for_a_non_script_name_is_not_this_tiers() {
        let p = LoadedPlugin {
            manifest: PluginManifest {
                name: "a-tcl-one".into(),
                kind: "tcl".into(),
                provides_hooks: Vec::new(),
                entry: "plugin.tcl".into(),
                description: String::new(),
                api: None,
            },
            dir: PathBuf::from("/nowhere"),
        };
        assert!(request_in("a-tcl-one", "unload", vec![p.clone()]).is_none());
        assert!(request_in("missing", "start", vec![p]).is_none());
        assert!(rows_in(&[]).is_empty());
    }
}
