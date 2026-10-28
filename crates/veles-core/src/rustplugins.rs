//! Rust plugins — in-tree trait registration (Phase A) and the host
//! for out-of-tree native `.so` plugins on C ABI 4 (R8 / M8).
//!
//! Tcl is the general-purpose scripting surface (everything is
//! scriptable from it); native plugins exist for the places performance
//! and static typing pay: hot hooks, tools with real state, filters
//! that run on every inbound message, a platform with its own wire.
//! Both honor the same verdict contract — first non-empty answer wins
//! (hermes transform-hook semantics), automation announces itself via
//! notes.
//!
//! # ABI 4 (2026-10-09) replaces ABI 3
//!
//! A plugin directory carries `kind = "rust"` + `entry = "plugin.so"`
//! in its plugin.toml; `load_so` dlopens the library and reads its
//! `veles_plugin_abi_version` export BEFORE anything else: a plugin
//! that does not export it is an ABI 3 (or older) build and is refused
//! with a message that names the rebuild; so is any version but 4. No
//! plugin code runs for a refusal. Then `veles_plugin_init` runs on a
//! thread tagged `so:<name>` with the plugin's id, so everything it
//! registers — through core's owner-tracked registries (tools,
//! commands, providers) and the handle table in `soservices.rs`
//! (hooks, filters, platforms, timers, sockets) — is the plugin's,
//! including what it registers LATER from a callback (the id names the
//! owner; the thread does not have to).
//!
//! `unload_so` is the M3 teardown for this tier: every registry entry
//! and slot the owner holds is purged (`modulehost::
//! purge_owner_everywhere`), its platforms' routes are released and its
//! timers and sockets aborted (`soservices::purge_owner`), the name is
//! kept out of the watcher's reload until `start_so`, and the library
//! is unmapped when nothing holds it — or kept mapped, with the count,
//! when a callback is still on a blocking thread. `veles plugins
//! unload|start|reload <name>` reach it through the tier dispatcher.
//!
//! Integration points (each one honest or the feature does not ship):
//!   - hooks: `HookBus::fire` consults rust handlers FIRST (on a
//!     blocking thread), then the Tcl plugins. Works with `tcl: None`.
//!   - tools: plugin tools ride `register_global_tool`, visible to
//!     sessions created after registration; an ABI 4 tool may answer
//!     with a future the host polls (`SoFuture`) instead of a string.
//!   - gateway filters: consulted after the Tcl `pre_gateway_dispatch`
//!     hook, in registration order, same skip/rewrite/allow shape.
//!   - platforms, timers, sockets: `soservices.rs`.
//!
//! Handler contract: SYNC, and the engine runs handlers where blocking
//! is allowed — `HookBus::fire`, the gateway's filter site, `SoTool::
//! call`, `render_plugin_async`, a platform's `deliver`, timer and
//! socket callbacks all reach a plugin from a blocking thread. So a
//! handler may take its time, and may call `host_call`, which blocks
//! on the bridge's runtime. The one exception is a future's `poll`,
//! which runs on an executor thread and must not block.
//!
//! Panics: a Rust `extern "C"` function that unwinds aborts, and the
//! host cannot catch it — `catch_unwind` cannot see into the plugin's
//! frame. The `veles-plugin-abi-sdk` crate catches every panic on the
//! plugin's side of the boundary; a plugin on the raw ABI is on its
//! own, and the ABI docs say so.

use crate::tools::Tool;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::{Arc, Mutex, OnceLock};
use veles_plugin_abi::{VelesHandle, VelesPluginId};

/// A hook handler: args JSON in, optional verdict out.
pub type RustHookFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What a gateway filter decided about an inbound message.
/// Mirrors gateway.rs's private `GatewayDecision` — the conversion is
/// one match at the call site, the module boundary stays clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterDecision {
    Skip,
    Rewrite(String),
    Allow,
}

/// A gateway filter: (platform, chat_id, text) in, optional decision.
pub type GatewayFilterFn = Arc<dyn Fn(&str, &str, &str) -> Option<FilterDecision> + Send + Sync>;

/// What a module's `start` receives: its place in the gateway (M3).
///
/// The module host builds one per instance on the instance's runtime and
/// hands it to `VelesPlugin::start`. It carries:
/// - the instance's identity (`plugin`, `instance`), for notes and logs;
/// - the config sections the manifest named (same shape as the process
///   tier's `InitParams::config`);
/// - the instance's private dirs (`Dirs`, made 0700 by the host);
/// - the instance runtime's handle, so module code can spawn onto it
///   WITHOUT touching `tokio::spawn` (which would land on the instance
///   runtime anyway from an instance thread, but a module that wants to
///   be explicit, or to spawn from a thread that is NOT an instance
///   thread — a bare std::thread it made, which the lints forbid for
///   in-tree modules — uses this handle).
///
/// Unlike the process tier, registration is not on this context: a
/// module calls core's ordinary API (`tools::register_global_tool`,
/// `commands::register_command`, …) from its `start`, and the owner tag
/// on the instance thread attributes every registration (M4). That is
/// the whole point of the shared core: the module uses the same API core
/// uses, not a guest protocol.
#[derive(Clone)]
pub struct ModuleContext {
    pub plugin: String,
    pub instance: String,
    pub config: std::collections::BTreeMap<String, serde_json::Value>,
    pub dirs: veles_types::plugin::Dirs,
    pub handle: tokio::runtime::Handle,
    /// The gateway's bus (R4): a platform module registers its outbound
    /// route and publishes its inbound on THIS, not on a handle of its
    /// own — the module host hands every instance the same bus the
    /// gateway runs.
    pub bus: crate::bus::GatewayBus,
    /// The gateway's store (R4, USER-chosen seam): a platform module
    /// parks it for its own reads (IRC's `!history` and trigger
    /// context) rather than the composition root parking it from the
    /// other side of the dependency. `None` for a host with no store
    /// (a test harness); a module that needs it says so, never guesses.
    pub store: Option<std::sync::Arc<std::sync::Mutex<crate::store::Store>>>,
}

/// A module's context carries a bus and a store, neither of which is
/// `Debug`; the context's debug shows the identity, not the handles.
impl std::fmt::Debug for ModuleContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleContext")
            .field("plugin", &self.plugin)
            .field("instance", &self.instance)
            .field("config", &self.config)
            .field("dirs", &self.dirs)
            .field("handle", &self.handle)
            .field("bus", &"<bus>")
            .field("store", &self.store.as_ref().map(|_| "<store>"))
            .finish()
    }
}

impl ModuleContext {
    /// The owner tag the instance's threads carry (`module:<plugin>[:<instance>]`).
    pub fn owner_tag(&self) -> String {
        if self.instance.is_empty() {
            format!("module:{}", self.plugin)
        } else {
            format!("module:{}:{}", self.plugin, self.instance)
        }
    }
}

/// A Rust plugin. Every method except name/version has a no-op default
/// so a plugin provides exactly the surfaces it needs.
pub trait VelesPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    /// (hook_name, handler) pairs. Hook names are validated against
    /// `hooks::KNOWN_HOOKS` at registration — a typo must not register
    /// a handler nothing can ever fire.
    fn hooks(&self) -> Vec<(String, RustHookFn)> {
        Vec::new()
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn gateway_filter(&self) -> Option<GatewayFilterFn> {
        None
    }
    /// Every gateway filter the plugin holds, asked in order until one
    /// decides. An ABI 4 plugin may hold several; an in-tree plugin's
    /// one `gateway_filter` is the default.
    fn gateway_filters(&self) -> Vec<GatewayFilterFn> {
        self.gateway_filter().into_iter().collect()
    }

    /// An in-process MODULE (M2) starts its work here. The module host
    /// calls it once, on the instance's runtime, after dlopen and
    /// registration. The default does nothing: a native (ABI 4) plugin
    /// or a registration-only module has no work to start.
    ///
    /// The future runs INSIDE the instance's dispatch gate: until it
    /// returns the instance is `starting`, and it must return within the
    /// host's start timeout (30 s) or the host tears the instance down
    /// and restarts it with back-off. A module that serves forever (an
    /// IRC connection loop) launches its tasks onto `cx.handle` and
    /// returns; the tasks then live on the instance runtime and are what
    /// `runtime.shutdown_timeout` drops at teardown.
    fn start(
        &self,
        _cx: ModuleContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    /// A courtesy, with a deadline: the host is about to tear the
    /// instance down. A platform module sends its QUIT here. Teardown
    /// does NOT depend on it: after the deadline the host purges the
    /// instance's registrations and shuts its runtime down regardless
    /// (M3). The default is to have nothing to say.
    fn stop(
        &self,
        _reason: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// One registered plugin. Its hooks and filters are read LIVE from the
/// plugin on every fire, so an ABI 4 plugin that registers a hook from
/// a callback (or drops one) is seen at once; the hook names were
/// validated against the taxonomy when they were registered.
struct Registered {
    plugin: Arc<dyn VelesPlugin>,
}

/// The rust-plugin registry. OWNER-TRACKED (M4/R2): each plugin records
/// the registering thread's owner, so a module's or a `.so`'s plugins
/// can be purged on reload without touching core's. Core/test
/// registrations on untagged threads → owner `None`, never purged.
/// Keyed by plugin name (the register/replace-by-name contract below).
static PLUGINS: std::sync::OnceLock<crate::owned::OwnedRegistry<String, Registered>> =
    std::sync::OnceLock::new();

fn plugins_cell() -> &'static crate::owned::OwnedRegistry<String, Registered> {
    PLUGINS.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Register a plugin. Idempotent per name: re-registering (a test
/// booting twice, a reload) replaces the previous instance instead of
/// stacking handlers.
///
/// Side effects at registration:
///   - tools ride `register_global_tool` (future sessions see them),
///   - hook names merge into the `hooks::REGISTERED` fast-path gate so
///     `has_handler` answers true without a Tcl registry,
///   - each plugin parks a plugin-action note — registration is
///     automation the user did not ask for in a turn.
///
/// An in-tree plugin's unknown hook names are refused here with a
/// warning (an ABI 4 plugin's are refused at `register_hook`, so its
/// list is already clean).
pub fn register_plugin(plugin: Arc<dyn VelesPlugin>) {
    let name = plugin.name().to_string();

    for (hook, _) in plugin.hooks() {
        if !crate::hooks::KNOWN_HOOKS.contains(&hook.as_str()) {
            tracing::warn!(
                "rust plugin `{name}`: unknown hook `{hook}` — handler NOT fired \
                 (see hooks::KNOWN_HOOKS)"
            );
        }
    }

    for tool in plugin.tools() {
        tracing::info!("rust plugin `{name}`: registering tool `{}`", tool.name());
        crate::tools::register_global_tool(tool);
    }

    plugins_cell().insert(name.clone(), Registered { plugin });
    crate::hooks::publish_rust_hook_names(rust_hook_names());
    crate::notes::record_plugin_note(
        &format!("rust plugin `{name}` registered"),
        "hooks + tools active; tools are visible to sessions created after registration",
    );
    tracing::info!("rust plugin registered: {name}");
}

/// Remove every plugin `owner` registered (a module reload, a `.so`
/// unload). `None` (core) removes nothing. Returns the count removed.
/// The plugins' TOOLS go with them (`tools::purge_owner_tools`) — the
/// host calls both.
pub fn purge_owner_plugins(owner: Option<&'static str>) -> usize {
    let n = plugins_cell().purge(owner);
    crate::hooks::publish_rust_hook_names(rust_hook_names());
    n
}

/// All registered plugins (manager surfaces, tests).
pub fn plugins() -> Vec<Arc<dyn VelesPlugin>> {
    let mut out = Vec::new();
    plugins_cell().for_each(|_, r| out.push(r.value.plugin.clone()));
    out
}

/// The hook names rust plugins handle RIGHT NOW — merged into the hook
/// fast-path gate by `hooks::publish_rust_hook_names`. Only names the
/// taxonomy knows count.
pub fn rust_hook_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    plugins_cell().for_each(|_, reg| {
        for (hook, _) in reg.value.plugin.hooks() {
            if crate::hooks::KNOWN_HOOKS.contains(&hook.as_str()) && !names.contains(&hook) {
                names.push(hook);
            }
        }
    });
    names
}

/// Fire `name` on every rust handler, first non-empty verdict wins.
/// Called from `HookBus::fire` on a blocking thread.
pub fn run_hooks(name: &str, args_json: &str) -> Option<String> {
    if !crate::hooks::KNOWN_HOOKS.contains(&name) {
        return None;
    }
    let sid = crate::hooks::ambient_session(args_json);
    crate::tools::with_ambient_hook_session(sid.as_deref(), || {
        let mut verdict_out: Option<String> = None;
        plugins_cell().for_each(|_, reg| {
            if verdict_out.is_some() {
                return;
            }
            for (hook, handler) in reg.value.plugin.hooks() {
                if hook == name {
                    if let Some(verdict) = handler(args_json) {
                        if !verdict.is_empty() {
                            verdict_out = Some(verdict);
                            return;
                        }
                    }
                }
            }
        });
        verdict_out
    })
}

/// Ask every rust gateway filter, in registration order, until one
/// decides.
pub fn run_gateway_filters(platform: &str, chat_id: &str, text: &str) -> Option<FilterDecision> {
    let mut out: Option<FilterDecision> = None;
    plugins_cell().for_each(|_, reg| {
        if out.is_some() {
            return;
        }
        for f in reg.value.plugin.gateway_filters() {
            if let Some(d) = f(platform, chat_id, text) {
                out = Some(d);
                return;
            }
        }
    });
    out
}

pub fn has_rust_handler(name: &str) -> bool {
    crate::hooks::rust_hook_names().iter().any(|h| h == name)
}

pub fn has_gateway_filter() -> bool {
    let mut any = false;
    plugins_cell().for_each(|_, reg| {
        if !reg.value.plugin.gateway_filters().is_empty() {
            any = true;
        }
    });
    any
}

// ── the native (.so) tier ───────────────────────────────────────────

/// A plugin's `ctx` pointer, carried across threads unchanged.
#[derive(Clone, Copy)]
struct CtxPtr(*mut c_void);
// SAFETY: opaque to the host; the plugin promised it is safe to hand
// back from any thread (ABI docs, "Threading").
unsafe impl Send for CtxPtr {}
unsafe impl Sync for CtxPtr {}
impl CtxPtr {
    fn get(self) -> *mut c_void {
        self.0
    }
}

/// One loaded `.so`: the registry's view of it (`VelesPlugin`) plus
/// what the host fns need to attribute its late registrations.
pub struct SoPlugin {
    name: String,
    version: String,
    id: VelesPluginId,
    owner: &'static str,
    hooks: Mutex<Vec<(VelesHandle, String, RustHookFn)>>,
    filters: Mutex<Vec<(VelesHandle, GatewayFilterFn)>>,
    free: veles_plugin_abi::VelesFreeFn,
    lib: Arc<libloading::Library>,
}

impl SoPlugin {
    /// The owner tag everything this plugin registers carries.
    pub fn owner(&self) -> &'static str {
        self.owner
    }
    /// The host's id for it.
    pub fn id(&self) -> VelesPluginId {
        self.id
    }
}

impl VelesPlugin for SoPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        &self.version
    }
    fn hooks(&self) -> Vec<(String, RustHookFn)> {
        self.hooks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(_, n, f)| (n.clone(), f.clone()))
            .collect()
    }
    fn gateway_filters(&self) -> Vec<GatewayFilterFn> {
        self.filters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(_, f)| f.clone())
            .collect()
    }
}

/// The loaded plugins by id (the host fns' lookup) and by name.
fn loaded() -> &'static Mutex<HashMap<VelesPluginId, Arc<SoPlugin>>> {
    static L: OnceLock<Mutex<HashMap<VelesPluginId, Arc<SoPlugin>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn plugin_of(id: VelesPluginId) -> Option<Arc<SoPlugin>> {
    loaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&id)
        .cloned()
}

fn loaded_by_name(name: &str) -> Option<Arc<SoPlugin>> {
    loaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .find(|p| p.name == name)
        .cloned()
}

/// Names `unload_so` took out: every `load_rust_manifests` (the
/// watcher's included) skips them until `start_so` names one.
fn so_unloaded() -> &'static Mutex<BTreeSet<String>> {
    static U: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    U.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Why a discovered `.so` is not loaded (an ABI 3 build, a failed
/// init): the listing's `refused` rows.
fn so_refused() -> &'static Mutex<BTreeMap<String, String>> {
    static R: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Libraries an unload could not unmap (a callback still running): kept
/// mapped for the life of the process rather than unmapping live code.
fn kept_mapped() -> &'static Mutex<Vec<Arc<libloading::Library>>> {
    static K: OnceLock<Mutex<Vec<Arc<libloading::Library>>>> = OnceLock::new();
    K.get_or_init(|| Mutex::new(Vec::new()))
}

static NEXT_PLUGIN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The refusal an ABI 3 (or older) plugin gets. One sentence that
/// names the rebuild; the tests pin its parts.
pub fn abi_refusal(path: &std::path::Path, found: Option<u32>) -> String {
    match found {
        None => format!(
            "{}: exports no veles_plugin_abi_version — not a veles plugin, or an ABI 3 or \
             older one; veles {} loads ABI {} only: rebuild against veles-plugin-abi {}.x \
             (docs/plugins.md, \"Out-of-tree Rust\")",
            path.display(),
            env!("CARGO_PKG_VERSION"),
            veles_plugin_abi::VELES_PLUGIN_ABI_VERSION,
            veles_plugin_abi::VELES_PLUGIN_ABI_VERSION,
        ),
        Some(v) => format!(
            "{}: ABI {v} plugin; veles {} loads ABI {} only: rebuild against veles-plugin-abi \
             {}.x (docs/plugins.md, \"Out-of-tree Rust\")",
            path.display(),
            env!("CARGO_PKG_VERSION"),
            veles_plugin_abi::VELES_PLUGIN_ABI_VERSION,
            veles_plugin_abi::VELES_PLUGIN_ABI_VERSION,
        ),
    }
}

/// Load an out-of-tree `cdylib` plugin and register it; a plugin of
/// that name already loaded is unloaded first (a reload). Returns the
/// registered plugin so callers/tests can inspect it. Errors name the
/// offending symbol or step — a `.so` that half-loads is a lie, and
/// the refusal of an older ABI names the rebuild.
pub fn load_so(
    path: &std::path::Path,
    name: &str,
    version: &str,
) -> anyhow::Result<Arc<dyn VelesPlugin>> {
    use libloading::Symbol;
    use veles_plugin_abi::{
        SYMBOL_ABI_VERSION, SYMBOL_FREE, SYMBOL_INIT, VELES_PLUGIN_ABI_VERSION,
    };

    if loaded_by_name(name).is_some() {
        let note = unload_so(name)?;
        tracing::info!("rust .so plugin `{name}`: reloading ({note})");
    }

    let lib = Arc::new(
        unsafe { libloading::Library::new(path) }
            .map_err(|e| anyhow::anyhow!("dlopen {}: {e}", path.display()))?,
    );
    // The version export FIRST: a plugin without it is an ABI ≤ 3
    // build, and reading its init through the ABI 4 table would be UB.
    let found: Option<u32> = unsafe {
        lib.get::<veles_plugin_abi::VelesAbiVersionFn>(SYMBOL_ABI_VERSION)
            .ok()
            .map(|f| f())
    };
    match found {
        Some(VELES_PLUGIN_ABI_VERSION) => {}
        other => {
            let msg = abi_refusal(path, other);
            so_refused()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(name.to_string(), msg.clone());
            anyhow::bail!("{msg}");
        }
    }
    // Copy the fn pointers out; the Symbol borrows end at the block.
    let (init_fn, free_fn) = unsafe {
        let init: Symbol<veles_plugin_abi::VelesInitFn> = lib
            .get(SYMBOL_INIT)
            .map_err(|e| anyhow::anyhow!("{}: missing veles_plugin_init: {e}", path.display()))?;
        let free: Symbol<veles_plugin_abi::VelesFreeFn> = lib.get(SYMBOL_FREE).map_err(|e| {
            anyhow::anyhow!("{}: missing veles_plugin_free_string: {e}", path.display())
        })?;
        (*init, *free)
    };

    let id = NEXT_PLUGIN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let owner = crate::lifecycle::owner_tag(&format!("so:{name}"));
    let plugin = Arc::new(SoPlugin {
        name: name.to_string(),
        version: version.to_string(),
        id,
        owner,
        hooks: Mutex::new(Vec::new()),
        filters: Mutex::new(Vec::new()),
        free: free_fn,
        lib: Arc::clone(&lib),
    });
    loaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id, Arc::clone(&plugin));

    // Init under the owner tag: every registration it makes through
    // core's registries is attributed; the id covers the late ones.
    let rc = crate::lifecycle::with_thread_owner(owner, || unsafe { init_fn(&API, id) });
    if rc != 0 {
        loaded()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        let purged = crate::modulehost::purge_owner_everywhere(Some(owner))
            + crate::soservices::purge_owner(owner);
        let msg = format!(
            "{}: veles_plugin_init returned {rc} ({purged} registration(s) it made were purged)",
            path.display()
        );
        so_refused()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string(), msg.clone());
        anyhow::bail!("{msg}");
    }

    crate::lifecycle::with_thread_owner(owner, || {
        register_plugin(Arc::clone(&plugin) as Arc<dyn VelesPlugin>)
    });
    so_unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(name);
    so_refused()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(name);
    Ok(plugin)
}

/// The M3 teardown for a `.so`: purge everything it owns, release its
/// platforms, timers and sockets, keep it out of the watcher's reload,
/// and unmap the library when nothing holds it. Answers the line the
/// surfaces print.
pub fn unload_so(name: &str) -> anyhow::Result<String> {
    let Some(plugin) = loaded_by_name(name) else {
        anyhow::bail!("no native plugin `{name}` is loaded");
    };
    loaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&plugin.id);
    let owner = plugin.owner;
    let purged = crate::modulehost::purge_owner_everywhere(Some(owner));
    let services = crate::soservices::purge_owner(owner);
    crate::hooks::publish_rust_hook_names(rust_hook_names());
    so_unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(name.to_string());

    // Quiescence: the registries dropped their `Arc<Library>` clones
    // with the purge; a callback still on a blocking thread holds one
    // until it returns. Wait a bounded while, then decide.
    let lib = Arc::clone(&plugin.lib);
    drop(plugin);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while Arc::strong_count(&lib) > 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let holders = Arc::strong_count(&lib) - 1;
    let what = format!("{purged} registration(s) purged, {services} host resource(s) released");
    if holders == 0 {
        drop(lib); // dlclose
        let line = format!("{name}: unloaded ({what}); code unmapped; `start` loads it again");
        tracing::info!("rust .so plugin {line}");
        Ok(line)
    } else {
        kept_mapped()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(lib);
        let line = format!(
            "{name}: unloaded ({what}); code kept mapped: {holders} callback(s) still running; \
             `start` loads it again"
        );
        tracing::warn!("rust .so plugin {line}");
        Ok(line)
    }
}

/// Load (again) the discovered `.so` plugin `name` — the `start` and
/// `reload` verbs. A loaded one is reloaded.
pub fn start_so(name: &str) -> anyhow::Result<String> {
    let cfg = crate::config::load().unwrap_or_default();
    let discovered = crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled)?;
    let Some(p) = discovered
        .iter()
        .find(|p| p.manifest.kind == "rust" && p.manifest.name == name)
    else {
        anyhow::bail!("no enabled native plugin named `{name}`");
    };
    let entry = p.dir.join(&p.manifest.entry);
    so_unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(name);
    match load_so(&entry, name, "so") {
        Ok(_) => Ok(format!("{name}: loaded ({})", entry.display())),
        Err(e) => Err(e),
    }
}

/// True when `unload_so` took `name` out and nothing loaded it since.
pub fn is_so_unloaded(name: &str) -> bool {
    so_unloaded()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(name)
}

/// The native tier's rows for the listings: every enabled `kind =
/// "rust"` manifest as `loaded`, `unloaded` or `refused` (with the
/// reason), plus any `.so` loaded from a dev dir.
pub fn so_rows() -> Vec<(String, &'static str, Option<String>)> {
    let cfg = crate::config::load().unwrap_or_default();
    let discovered =
        crate::plugins::discover(&cfg.plugins.enabled, &cfg.plugins.disabled).unwrap_or_default();
    let refused = so_refused()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let mut rows: Vec<(String, &'static str, Option<String>)> = Vec::new();
    for p in discovered.iter().filter(|p| p.manifest.kind == "rust") {
        let n = p.manifest.name.clone();
        let row = if loaded_by_name(&n).is_some() {
            (n, "loaded", None)
        } else if is_so_unloaded(&n) {
            (n, "unloaded", None)
        } else if let Some(why) = refused.get(&n) {
            (n, "refused", Some(why.clone()))
        } else {
            (n, "unloaded", None)
        };
        rows.push(row);
    }
    let named: Vec<String> = rows.iter().map(|(n, _, _)| n.clone()).collect();
    for p in loaded().lock().unwrap_or_else(|p| p.into_inner()).values() {
        if !named.contains(&p.name) {
            rows.push((p.name.clone(), "loaded", None));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

// ── the host's function table ───────────────────────────────────────

/// A `'static`, not a local. This MATTERS: a plugin keeps the pointer so
/// its handlers can call `host_call` later, and handing it `&some_local`
/// gives it a dangling pointer the moment `load_so` returns. Every field
/// is a `u32` or a function pointer, so the whole struct is a
/// compile-time constant with a `'static` address.
static API: veles_plugin_abi::VelesHostApi = veles_plugin_abi::VelesHostApi {
    abi_version: veles_plugin_abi::VELES_PLUGIN_ABI_VERSION,
    struct_size: std::mem::size_of::<veles_plugin_abi::VelesHostApi>(),
    register_hook: host_register_hook,
    register_tool: host_register_tool,
    register_command: host_register_command,
    register_provider: host_register_provider,
    register_gateway_filter: host_register_gateway_filter,
    register_platform: host_register_platform,
    unregister: host_unregister,
    platform_inbound: host_platform_inbound,
    timer_after: host_timer_after,
    timer_cancel: host_timer_cancel,
    tcp_connect: host_tcp_connect,
    tcp_send: host_tcp_send,
    tcp_close: host_tcp_close,
    host_call,
    host_free_string,
    register_json: host_register_json,
};

/// # Safety
/// `p` is NULL or a NUL-terminated string valid for the call.
unsafe fn read_str(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Wrap a plugin-returned string: copy it out and hand it back to the
/// plugin's deallocator. NULL → None.
unsafe fn take_plugin_string(
    out: *mut c_char,
    free: veles_plugin_abi::VelesFreeFn,
) -> Option<String> {
    if out.is_null() {
        return None;
    }
    let s = CStr::from_ptr(out).to_string_lossy().into_owned();
    free(out);
    Some(s)
}

unsafe extern "C" fn host_register_hook(
    plugin: VelesPluginId,
    hook_name: *const c_char,
    handler: veles_plugin_abi::VelesHookFn,
    ctx: *mut c_void,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        tracing::warn!("register_hook from an unknown plugin id {plugin}");
        return 0;
    };
    let name = read_str(hook_name);
    if !crate::hooks::KNOWN_HOOKS.contains(&name.as_str()) {
        tracing::warn!(
            "rust .so plugin `{}`: unknown hook `{name}` — handler NOT registered \
             (see hooks::KNOWN_HOOKS)",
            p.name
        );
        return 0;
    }
    let lib = Arc::clone(&p.lib);
    let free = p.free;
    let ctx = CtxPtr(ctx);
    let f: RustHookFn = Arc::new(move |args: &str| -> Option<String> {
        let _keep = &lib;
        let c_args = CString::new(args.replace('\0', "")).ok()?;
        let out = unsafe { handler(c_args.as_ptr(), ctx.get()) };
        unsafe { take_plugin_string(out, free) }.filter(|s| !s.is_empty())
    });
    let handle = crate::soservices::insert(plugin, p.owner, crate::soservices::Resource::Hook);
    p.hooks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle, name, f));
    crate::hooks::publish_rust_hook_names(rust_hook_names());
    handle
}

unsafe extern "C" fn host_register_tool(
    plugin: VelesPluginId,
    spec: *const veles_plugin_abi::VelesToolSpec,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        tracing::warn!("register_tool from an unknown plugin id {plugin}");
        return 0;
    };
    if spec.is_null() {
        return 0;
    }
    let spec = &*spec;
    let name = read_str(spec.name);
    if name.is_empty() {
        tracing::warn!("rust .so plugin `{}`: tool with empty name skipped", p.name);
        return 0;
    }
    if spec.call.is_none() && spec.call_async.is_none() {
        tracing::warn!(
            "rust .so plugin `{}`: tool `{name}` sets neither call nor call_async — skipped",
            p.name
        );
        return 0;
    }
    let schema_json = read_str(spec.schema_json);
    let schema = serde_json::from_str(&schema_json).unwrap_or_else(|_| {
        tracing::warn!(
            "rust .so plugin `{}`: tool `{name}` has invalid schema JSON; using empty object",
            p.name
        );
        serde_json::json!({"type": "object"})
    });
    let tool = Arc::new(SoTool {
        name: name.clone(),
        description: read_str(spec.description),
        schema,
        call: spec.call,
        call_async: spec.call_async,
        ctx: CtxPtr(spec.ctx),
        free: p.free,
        _lib: Arc::clone(&p.lib),
    });
    crate::lifecycle::with_thread_owner(p.owner, || {
        crate::tools::register_global_tool(tool);
    });
    tracing::info!("rust .so plugin `{}`: registered tool `{name}`", p.name);
    crate::soservices::insert(plugin, p.owner, crate::soservices::Resource::Tool(name))
}

/// `register_json` (PLAN-extension-api.md D2): one registrar for every
/// JSON-shaped kind, so a new kind is a new string, not a new table
/// field. Today's one kind is `service`: a service other plugins (or
/// core) call by name, which is also how a `.so` overrides a point.
unsafe extern "C" fn host_register_json(
    plugin: VelesPluginId,
    kind: *const c_char,
    name: *const c_char,
    decl_json: *const c_char,
    cb: veles_plugin_abi::VelesJsonFn,
    ctx: *mut c_void,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        tracing::warn!("register_json from an unknown plugin id {plugin}");
        return 0;
    };
    let kind = read_str(kind);
    let name = read_str(name);
    if kind != veles_plugin_abi::JSON_KIND_SERVICE {
        tracing::warn!(
            "rust .so plugin `{}`: register_json kind `{kind}` is not one this host knows \
             (`{}`) — `{name}` NOT registered",
            p.name,
            veles_plugin_abi::JSON_KIND_SERVICE
        );
        return 0;
    }
    let decl: serde_json::Value =
        serde_json::from_str(&read_str(decl_json)).unwrap_or(serde_json::Value::Null);
    let sdecl = crate::services::ServiceDecl {
        name: name.clone(),
        description: decl["description"].as_str().unwrap_or("").to_string(),
        cap: decl["cap"]
            .as_str()
            .filter(|c| !c.trim().is_empty())
            .map(str::to_string),
    };
    let lib = Arc::clone(&p.lib);
    let free = p.free;
    let ctx = CtxPtr(ctx);
    let handler = crate::services::handler(move |params, cx| {
        let lib = Arc::clone(&lib);
        let input = crate::services::script_payload(&params, &cx).to_string();
        async move {
            crate::rt::spawn_blocking(move || {
                let _keep = &lib;
                let c_input = CString::new(input.replace('\0', ""))
                    .map_err(|e| format!("the service input held a NUL: {e}"))?;
                // SAFETY: the plugin's callback, its ctx, and a string
                // valid for the call; the answer is the plugin's to free.
                let out = unsafe { cb(c_input.as_ptr(), ctx.get()) };
                let text = unsafe { take_plugin_string(out, free) }
                    .ok_or_else(|| "the plugin's service answered NULL".to_string())?;
                // The SDK's error lane (an `Err`, a caught panic).
                if text.starts_with("error:") {
                    return Err(text);
                }
                Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
            })
            .await
            .map_err(|e| format!("the plugin's service callback was cancelled: {e}"))?
        }
    });
    let r =
        crate::lifecycle::with_thread_owner(p.owner, || crate::services::register(sdecl, handler));
    if let Err(e) = r {
        tracing::warn!(
            "rust .so plugin `{}`: service `{name}` refused: {e}",
            p.name
        );
        return 0;
    }
    tracing::info!("rust .so plugin `{}`: provides service `{name}`", p.name);
    crate::soservices::insert(plugin, p.owner, crate::soservices::Resource::Service(name))
}

unsafe extern "C" fn host_register_command(
    plugin: VelesPluginId,
    spec: *const veles_plugin_abi::VelesCommandSpec,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        tracing::warn!("register_command from an unknown plugin id {plugin}");
        return 0;
    };
    if spec.is_null() {
        return 0;
    }
    let spec = &*spec;
    let name = read_str(spec.name);
    if name.is_empty() {
        tracing::warn!(
            "rust .so plugin `{}`: command with empty name skipped",
            p.name
        );
        return 0;
    }
    // Unlike hooks and filters a command does NOT ride the plugin
    // struct: `commands::register_command` is a process-global registry
    // with its own replace-by-name semantics, the same one
    // `veles::register_command` writes into, so a `.so` command and a
    // Tcl command are indistinguishable to every frontend.
    let lib = Arc::clone(&p.lib);
    let free = p.free;
    let ctx = CtxPtr(spec.ctx);
    let render = spec.render;
    let cmd_name = name.clone();
    let body: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(move |args: &str| {
        let _keep = &lib;
        let Ok(c_args) = CString::new(args.replace('\0', "")) else {
            return format!("error: /{cmd_name} got arguments containing a NUL");
        };
        let out = unsafe { render(c_args.as_ptr(), ctx.get()) };
        unsafe { take_plugin_string(out, free) }.unwrap_or_default()
    });
    let r = crate::lifecycle::with_thread_owner(p.owner, || {
        crate::commands::register_command(crate::commands::PluginCommand {
            name: name.clone(),
            description: read_str(spec.description),
            body: crate::commands::CommandBody::Native(body),
        })
    });
    if let Err(e) = r {
        tracing::warn!(
            "rust .so plugin `{}`: command `{name}` rejected: {e}",
            p.name
        );
        return 0;
    }
    crate::soservices::insert(plugin, p.owner, crate::soservices::Resource::Command(name))
}

unsafe extern "C" fn host_register_provider(
    plugin: VelesPluginId,
    spec: *const veles_plugin_abi::VelesProviderSpec,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        tracing::warn!("register_provider from an unknown plugin id {plugin}");
        return 0;
    };
    if spec.is_null() {
        return 0;
    }
    let spec = &*spec;
    let name = read_str(spec.name);
    if name.is_empty() {
        tracing::warn!(
            "rust .so plugin `{}`: provider driver with empty name skipped",
            p.name
        );
        return 0;
    }
    // Like commands, drivers live in a process-global registry
    // (providers::register_driver): a driver from a `.so` and one from a
    // Tcl proc are the same thing to `build_client`.
    let lib = Arc::clone(&p.lib);
    let free = p.free;
    let ctx = CtxPtr(spec.ctx);
    let chat = spec.chat;
    let driver_name = name.clone();
    let call: crate::providers::ProviderDriverFn = Arc::new(
        move |body: &serde_json::Value,
              _emit: &mut dyn FnMut(&serde_json::Value)|
              -> Result<serde_json::Value, String> {
            let _keep = &lib;
            // Chunks do NOT come back through `_emit`: a driver streams
            // by calling host_call("provider_emit", …), the same route
            // the Tcl side takes. One implementation, both languages.
            let c_body = CString::new(body.to_string().replace('\0', ""))
                .map_err(|_| "request body contained a NUL".to_string())?;
            let out = unsafe { chat(c_body.as_ptr(), ctx.get()) };
            let Some(s) = (unsafe { take_plugin_string(out, free) }) else {
                return Err(format!(
                    "driver `{driver_name}` returned NULL — answer an OpenAI-shaped \
                     response object, or {{\"error\":\"...\"}}"
                ));
            };
            serde_json::from_str(&s)
                .map_err(|e| format!("driver `{driver_name}` did not answer JSON: {e}"))
        },
    );
    let r = crate::lifecycle::with_thread_owner(p.owner, || {
        crate::providers::register_driver(crate::providers::ProviderDriver {
            name: name.clone(),
            plugin: format!("so:{}", p.name),
            call,
        })
    });
    if let Err(e) = r {
        tracing::warn!(
            "rust .so plugin `{}`: provider driver `{name}` rejected: {e}",
            p.name
        );
        return 0;
    }
    crate::soservices::insert(plugin, p.owner, crate::soservices::Resource::Provider(name))
}

unsafe extern "C" fn host_register_gateway_filter(
    plugin: VelesPluginId,
    filter_fn: veles_plugin_abi::VelesFilterFn,
    ctx: *mut c_void,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        tracing::warn!("register_gateway_filter from an unknown plugin id {plugin}");
        return 0;
    };
    let lib = Arc::clone(&p.lib);
    let free = p.free;
    let ctx = CtxPtr(ctx);
    let f: GatewayFilterFn = Arc::new(
        move |platform: &str, chat_id: &str, text: &str| -> Option<FilterDecision> {
            let _keep = &lib;
            let c_p = CString::new(platform.replace('\0', "")).ok()?;
            let c_c = CString::new(chat_id.replace('\0', "")).ok()?;
            let c_t = CString::new(text.replace('\0', "")).ok()?;
            let mut rewrite: *mut c_char = std::ptr::null_mut();
            let code = unsafe {
                filter_fn(
                    c_p.as_ptr(),
                    c_c.as_ptr(),
                    c_t.as_ptr(),
                    &mut rewrite,
                    ctx.get(),
                )
            };
            use veles_plugin_abi::*;
            match code {
                FILTER_SKIP => Some(FilterDecision::Skip),
                FILTER_ALLOW => Some(FilterDecision::Allow),
                FILTER_REWRITE => {
                    // A malformed rewrite (NULL) is no decision.
                    unsafe { take_plugin_string(rewrite, free) }.map(FilterDecision::Rewrite)
                }
                _ => None,
            }
        },
    );
    let handle = crate::soservices::insert(plugin, p.owner, crate::soservices::Resource::Filter);
    p.filters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle, f));
    handle
}

unsafe extern "C" fn host_register_platform(
    plugin: VelesPluginId,
    spec: *const veles_plugin_abi::VelesPlatformSpec,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        tracing::warn!("register_platform from an unknown plugin id {plugin}");
        return 0;
    };
    if spec.is_null() {
        return 0;
    }
    let spec = &*spec;
    let key = read_str(spec.key);
    let r = crate::lifecycle::with_thread_owner(p.owner, || {
        crate::soservices::register_platform(
            plugin,
            p.owner,
            Arc::clone(&p.lib),
            key.clone(),
            spec.public_feed != 0,
            spec.deliver,
            crate::soservices::Ctx(spec.ctx),
        )
    });
    match r {
        Ok(h) => {
            tracing::info!("rust .so plugin `{}`: platform `{key}` registered", p.name);
            h
        }
        Err(e) => {
            tracing::warn!(
                "rust .so plugin `{}`: platform `{key}` refused: {e}",
                p.name
            );
            0
        }
    }
}

unsafe extern "C" fn host_unregister(plugin: VelesPluginId, handle: VelesHandle) -> i32 {
    use crate::soservices::Resource;
    let Some(p) = plugin_of(plugin) else {
        return veles_plugin_abi::VELES_ERR_NO_SUCH_HANDLE;
    };
    let Some(entry) = crate::soservices::take(handle, plugin) else {
        return veles_plugin_abi::VELES_ERR_NO_SUCH_HANDLE;
    };
    let owner = Some(p.owner);
    match &entry.resource {
        Resource::Tool(name) => {
            crate::tools::remove_owned_tool(name, owner);
        }
        Resource::Command(name) => {
            crate::commands::remove_owned_command(name, owner);
        }
        Resource::Provider(name) => {
            crate::providers::remove_owned_driver(name, owner);
        }
        Resource::Service(name) => {
            crate::services::withdraw_owned(name, owner);
        }
        Resource::Hook => {
            p.hooks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(h, _, _)| *h != handle);
            crate::hooks::publish_rust_hook_names(rust_hook_names());
        }
        Resource::Filter => {
            p.filters
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(h, _)| *h != handle);
        }
        Resource::Platform { .. } | Resource::Timer(_) => crate::soservices::release(entry),
        Resource::Socket { tx, .. } => {
            // Polite: the task shuts the stream and delivers CLOSED.
            let _ = tx.send(crate::soservices::SockCmd::Close);
        }
    }
    veles_plugin_abi::VELES_OK
}

unsafe extern "C" fn host_platform_inbound(plugin: VelesPluginId, json: *const c_char) -> i32 {
    if json.is_null() {
        return veles_plugin_abi::VELES_ERR_BAD_ARGUMENT;
    }
    let json = read_str(json);
    crate::soservices::platform_inbound(plugin, &json)
}

unsafe extern "C" fn host_timer_after(
    plugin: VelesPluginId,
    ms: u64,
    cb: veles_plugin_abi::VelesTimerFn,
    ctx: *mut c_void,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        return 0;
    };
    crate::lifecycle::with_thread_owner(p.owner, || {
        crate::soservices::timer_after(
            plugin,
            p.owner,
            Arc::clone(&p.lib),
            ms,
            cb,
            crate::soservices::Ctx(ctx),
        )
    })
}

unsafe extern "C" fn host_timer_cancel(plugin: VelesPluginId, handle: VelesHandle) -> i32 {
    crate::soservices::timer_cancel(plugin, handle)
}

unsafe extern "C" fn host_tcp_connect(
    plugin: VelesPluginId,
    host: *const c_char,
    port: u16,
    tls: u8,
    on_event: veles_plugin_abi::VelesSocketEventFn,
    ctx: *mut c_void,
) -> VelesHandle {
    let Some(p) = plugin_of(plugin) else {
        return 0;
    };
    let host = read_str(host);
    if host.is_empty() {
        return 0;
    }
    crate::lifecycle::with_thread_owner(p.owner, || {
        crate::soservices::tcp_connect(
            plugin,
            p.owner,
            Arc::clone(&p.lib),
            host,
            port,
            tls != 0,
            on_event,
            crate::soservices::Ctx(ctx),
        )
    })
}

unsafe extern "C" fn host_tcp_send(
    plugin: VelesPluginId,
    handle: VelesHandle,
    data: *const u8,
    len: usize,
) -> i32 {
    let bytes = if data.is_null() || len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data, len).to_vec()
    };
    crate::soservices::tcp_send(plugin, handle, bytes)
}

unsafe extern "C" fn host_tcp_close(plugin: VelesPluginId, handle: VelesHandle) -> i32 {
    crate::soservices::tcp_close(plugin, handle)
}

unsafe extern "C" fn host_call(op: *const c_char, args_json: *const c_char) -> *mut c_char {
    let answer = std::panic::catch_unwind(|| {
        if op.is_null() {
            return err_json("host_call: null op name");
        }
        let op = CStr::from_ptr(op).to_string_lossy().into_owned();
        let raw = if args_json.is_null() {
            "[]".to_string()
        } else {
            CStr::from_ptr(args_json).to_string_lossy().into_owned()
        };
        let args: Vec<String> =
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(serde_json::Value::Array(items)) => items
                    .iter()
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => String::new(),
                        other => other.to_string(),
                    })
                    .collect(),
                Ok(_) => return err_json(
                    "host_call: args_json must be a JSON ARRAY of strings, e.g. [\"sid\",\"5\"]",
                ),
                Err(e) => return err_json(&format!("host_call: args_json is not JSON: {e}")),
            };

        let Some(bridge) = crate::bridge::global() else {
            return err_json("host_call: no engine bridge (config unreadable)");
        };
        match crate::bridgeops::dispatch(&bridge, &op, &args) {
            Ok(out) => serde_json::json!({ "ok": out }).to_string(),
            Err(e) => serde_json::json!({ "error": e }).to_string(),
        }
    })
    .unwrap_or_else(|_| {
        err_json(
            "host_call failed at the boundary (most likely called from an async context, \
             where blocking is refused) — call engine ops from a handler: a hook, a tool, \
             a gateway filter, a command render, a provider driver, a timer or a socket \
             event. Those all run where blocking is allowed; a future's poll does not.",
        )
    });

    match CString::new(answer.replace('\0', "")) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

unsafe extern "C" fn host_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

// ── tools: sync on a blocking thread, or a future the host polls ────

struct SoTool {
    name: String,
    description: String,
    schema: serde_json::Value,
    call: Option<veles_plugin_abi::VelesToolCallFn>,
    call_async: Option<veles_plugin_abi::VelesToolCallAsyncFn>,
    ctx: CtxPtr,
    free: veles_plugin_abi::VelesFreeFn,
    _lib: Arc<libloading::Library>,
}

#[async_trait::async_trait]
impl Tool for SoTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }
    async fn call(&self, args: serde_json::Value) -> String {
        let args_json = args.to_string();
        let ctx = self.ctx;
        let free = self.free;
        let lib = Arc::clone(&self._lib);
        if let Some(call_async) = self.call_async {
            // The plugin builds its future inline (cheap by contract);
            // the host polls it as a native future from here on.
            let c_args = match CString::new(args_json.replace('\0', "")) {
                Ok(c) => c,
                Err(_) => return "error: tool args contained NUL".to_string(),
            };
            let vf = unsafe { call_async(c_args.as_ptr(), ctx.get()) };
            if vf.is_null() {
                return "error: the plugin could not start the call (NULL future)".to_string();
            }
            return SoFuture::new(vf, free, lib).await;
        }
        let Some(call) = self.call else {
            return "error: the tool has no call function".to_string();
        };
        let res = crate::rt::spawn_blocking(move || {
            let _keep = &lib; // code stays resident for the call
            let c_args = match CString::new(args_json.replace('\0', "")) {
                Ok(c) => c,
                Err(_) => return "error: tool args contained NUL".to_string(),
            };
            let out = unsafe { call(c_args.as_ptr(), ctx.get()) };
            unsafe { take_plugin_string(out, free) }.unwrap_or_default()
        })
        .await;
        res.unwrap_or_else(|e| format!("error: plugin tool join failed: {e}"))
    }
}

/// The host's side of a plugin future: the waker the plugin is handed
/// is a reference-counted slot holding the executor's current waker.
struct WakeSlot {
    waker: Mutex<Option<std::task::Waker>>,
}

unsafe extern "C" fn slot_clone(data: *mut c_void) -> *mut c_void {
    Arc::increment_strong_count(data as *const WakeSlot);
    data
}
unsafe extern "C" fn slot_wake(data: *mut c_void) {
    let slot = &*(data as *const WakeSlot);
    let w = slot.waker.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(w) = w {
        w.wake();
    }
}
unsafe extern "C" fn slot_drop(data: *mut c_void) {
    Arc::decrement_strong_count(data as *const WakeSlot);
}

/// A `VelesFuture` as a Rust future. Dropping it before it is ready
/// cancels the plugin's future (its `drop` runs exactly once).
struct SoFuture {
    vf: *mut veles_plugin_abi::VelesFuture,
    slot: Arc<WakeSlot>,
    free: veles_plugin_abi::VelesFreeFn,
    _lib: Arc<libloading::Library>,
    done: bool,
}
// SAFETY: the plugin promised its future may be polled from any thread
// (ABI docs); the host polls it from one task at a time.
unsafe impl Send for SoFuture {}

impl SoFuture {
    fn new(
        vf: *mut veles_plugin_abi::VelesFuture,
        free: veles_plugin_abi::VelesFreeFn,
        lib: Arc<libloading::Library>,
    ) -> Self {
        Self {
            vf,
            slot: Arc::new(WakeSlot {
                waker: Mutex::new(None),
            }),
            free,
            _lib: lib,
            done: false,
        }
    }
}

impl std::future::Future for SoFuture {
    type Output = String;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<String> {
        let this = self.get_mut();
        if this.done {
            return std::task::Poll::Ready("error: the future was polled after it ended".into());
        }
        *this.slot.waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(cx.waker().clone());
        // One reference for the borrowed waker the plugin sees during
        // this poll; released below. Its clones are the plugin's.
        let data = Arc::into_raw(Arc::clone(&this.slot)) as *mut c_void;
        let vw = veles_plugin_abi::VelesWaker {
            clone: slot_clone,
            wake: slot_wake,
            drop: slot_drop,
            data,
        };
        let vf = unsafe { &*this.vf };
        let ready = unsafe { (vf.poll)(vf.ctx, &vw) };
        unsafe { Arc::decrement_strong_count(data as *const WakeSlot) };
        if ready == 0 {
            return std::task::Poll::Pending;
        }
        let out = unsafe { (vf.take)(vf.ctx) };
        let s = unsafe { take_plugin_string(out, this.free) }
            .unwrap_or_else(|| "error: the plugin's future answered NULL".to_string());
        this.done = true;
        // Exactly once; nothing touches the future after.
        unsafe { (vf.drop)(vf.ctx) };
        std::task::Poll::Ready(s)
    }
}

impl Drop for SoFuture {
    fn drop(&mut self) {
        if !self.done {
            self.done = true;
            let vf = unsafe { &*self.vf };
            unsafe { (vf.drop)(vf.ctx) };
        }
    }
}

/// Load every `kind = "rust"` manifest under the discovered set. Called
/// from the plugin loader (tclrt) alongside the Tcl sourcing — on every
/// load, the watcher's included, so a loaded plugin is RELOADED by it
/// (as a Tcl plugin is re-sourced) and an unloaded one stays out.
/// Failures are logged and recorded for the listing, not fatal — one
/// bad `.so` must not stop the rest.
pub fn load_rust_manifests(discovered: &[crate::plugins::LoadedPlugin]) {
    for p in discovered.iter().filter(|p| p.manifest.kind == "rust") {
        if is_so_unloaded(&p.manifest.name) {
            tracing::info!("rust plugin {}: unloaded; not loaded", p.manifest.name);
            continue;
        }
        let entry = p.dir.join(&p.manifest.entry);
        if !entry.exists() {
            tracing::warn!(
                "rust plugin {} missing entry {}",
                p.manifest.name,
                entry.display()
            );
            continue;
        }
        if let Err(e) = load_so(&entry, &p.manifest.name, "so") {
            so_refused()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(p.manifest.name.clone())
                .or_insert_with(|| format!("{e:#}"));
            tracing::error!("rust plugin {} failed to load: {e:#}", p.manifest.name);
        } else {
            tracing::info!("loaded rust .so plugin: {}", p.manifest.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookBus;

    struct TestPlugin;
    impl VelesPlugin for TestPlugin {
        fn name(&self) -> &str {
            "test-plugin"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn hooks(&self) -> Vec<(String, RustHookFn)> {
            vec![(
                "post_llm_call".to_string(),
                Arc::new(|args: &str| {
                    if args.contains("magic") {
                        Some(r#"{"decision":"saw-it"}"#.to_string())
                    } else {
                        None
                    }
                }),
            )]
        }
    }

    struct UnknownHookPlugin;
    impl VelesPlugin for UnknownHookPlugin {
        fn name(&self) -> &str {
            "unknown-hook-plugin"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn hooks(&self) -> Vec<(String, RustHookFn)> {
            vec![("definitely_not_a_hook".to_string(), Arc::new(|_| None))]
        }
    }

    /// Process-global registry: tests here run in one process, so each
    /// test registers under its OWN plugin name (register_plugin
    /// replaces same-name entries).
    #[test]
    fn registration_validates_hook_names() {
        register_plugin(Arc::new(UnknownHookPlugin));
        assert!(
            !has_rust_handler("definitely_not_a_hook"),
            "an unknown hook name must not register"
        );
        assert!(
            run_hooks("definitely_not_a_hook", "{}").is_none(),
            "and must never fire"
        );
    }

    #[test]
    fn rust_hooks_fire_and_first_verdict_wins() {
        register_plugin(Arc::new(TestPlugin));
        assert!(has_rust_handler("post_llm_call"));
        let verdict = run_hooks("post_llm_call", r#"{"content":"the magic word"}"#);
        assert_eq!(verdict.as_deref(), Some(r#"{"decision":"saw-it"}"#));
        // No opinion → None, and an unhandled hook is never consulted.
        assert!(run_hooks("post_llm_call", r#"{"content":"plain"}"#).is_none());
        assert!(run_hooks("pre_llm_call", "{}").is_none());
    }

    /// The whole point of the rust path: a bus WITHOUT a Tcl
    /// interpreter still reaches rust handlers (hooks.rs's old claim).
    #[tokio::test]
    async fn a_bus_without_tcl_still_reaches_rust_handlers() {
        register_plugin(Arc::new(TestPlugin));
        let bus = HookBus::new(None);
        assert!(!bus.has_tcl(), "this test needs the Tcl-less bus");
        let v = bus.fire("post_llm_call", r#"{"content":"magic"}"#).await;
        assert_eq!(v.as_deref(), Some(r#"{"decision":"saw-it"}"#));
    }

    #[test]
    fn re_registering_replaces_not_stacks() {
        register_plugin(Arc::new(TestPlugin));
        register_plugin(Arc::new(TestPlugin));
        let count = plugins()
            .iter()
            .filter(|p| p.name() == "test-plugin")
            .count();
        assert_eq!(count, 1, "same-name registration must replace");
        // …and the handler still fires exactly once per fire().
        assert_eq!(
            run_hooks("post_llm_call", "magic").as_deref(),
            Some(r#"{"decision":"saw-it"}"#)
        );
    }
}

#[cfg(test)]
mod abi_tests {
    use super::*;

    /// Call `host_call` the way a `.so` does, straight through the
    /// function pointer the ABI hands over.
    fn call(op: &str, args_json: &str) -> String {
        let (c_op, c_args) = (
            CString::new(op).expect("op"),
            CString::new(args_json).expect("args"),
        );
        unsafe {
            let out = host_call(c_op.as_ptr(), c_args.as_ptr());
            assert!(
                !out.is_null(),
                "host_call must never answer NULL for `{op}`"
            );
            let s = CStr::from_ptr(out).to_string_lossy().into_owned();
            host_free_string(out);
            s
        }
    }

    /// PARITY, the other half of the taxonomy.
    ///
    /// `tclrt`'s api-stub test proves `bridgeops::OP_NAMES` matches what
    /// Tcl registers. This proves the same list is what the C ABI
    /// accepts — so an op cannot reach one plugin language and not the
    /// other, which is exactly the state ABI 1 shipped in.
    #[test]
    fn host_call_accepts_every_op_and_only_those() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");

        for op in crate::bridgeops::OP_NAMES {
            // No arguments: an op that needs some answers with its
            // usage line, an op that needs none answers normally.
            // Either is fine. What must NOT come back is the
            // unknown-op message, which is what a missing ABI arm
            // would produce.
            let answer = call(op, "[]");
            assert!(
                answer.starts_with('{') && answer.contains("\"ok\"")
                    || answer.contains("\"error\""),
                "`{op}` must answer the documented envelope, got: {answer}"
            );
            assert!(
                !answer.contains("unknown veles op"),
                "`{op}` is in OP_NAMES but host_call cannot reach it: {answer}"
            );
        }

        // The Tcl-only registrars are deliberately NOT reachable here:
        // their last argument is a proc name, and the C side registers
        // them through a function pointer instead. If one ever becomes
        // an op, this assertion is the thing that says so.
        for op in crate::bridgeops::TCL_ONLY_COMMANDS {
            let answer = call(op, "[]");
            assert!(
                answer.contains("unknown veles op"),
                "`{op}` is Tcl-only and must not be a host_call op: {answer}"
            );
        }

        let answer = call("no_such_op", "[]");
        assert!(answer.contains("\"error\""), "{answer}");
        assert!(answer.contains("no_such_op"), "{answer}");
    }

    /// THE load-bearing assumption of the whole ABI, checked rather
    /// than reasoned about.
    ///
    /// Every engine op that does I/O — `call_llm`, `http_get`,
    /// `http_post`, `tool_call` — blocks on the bridge's runtime, and
    /// tokio PANICS if you block from inside an async context rather
    /// than merely being slow. Plugin handlers are all invoked from
    /// `spawn_blocking` threads, which report a current runtime handle
    /// exactly like executor threads do; whether blocking is allowed
    /// there is not something the public API will tell you.
    ///
    /// It is allowed — a blocking-pool thread is not a scheduler
    /// context — so this asserts it. If it ever stops being true, a
    /// provider driver written in Rust cannot make an HTTP request, and
    /// the `catch_unwind` at the boundary turns the whole feature into
    /// an error message rather than a crash, which is worse: it would
    /// look like a plugin bug.
    #[tokio::test]
    async fn a_blocking_op_is_reachable_from_a_handler_thread() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");
        let answer = crate::rt::spawn_blocking(|| call("http_get", r#"["http://127.0.0.1:0/"]"#))
            .await
            .expect("join");
        // A closed port: the request must have been ATTEMPTED, which is
        // proof block_on ran. What must not appear is the boundary's
        // panic message.
        assert!(
            answer.contains("error sending request") || answer.contains("http"),
            "the op must have reached the network layer: {answer}"
        );
        assert!(
            !answer.contains("failed at the boundary"),
            "blocking from a handler thread must not panic: {answer}"
        );
    }

    /// A plugin can send junk, and junk must not be able to kill the
    /// agent. Every one of these used to be a way to reach a `panic` or
    /// an `unwrap` across a boundary where unwinding aborts the process.
    #[test]
    fn host_call_answers_malformed_input_instead_of_aborting() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("config");

        // Not an array.
        let a = call("json_get", r#"{"not":"an array"}"#);
        assert!(a.contains("must be a JSON ARRAY"), "{a}");
        // Not JSON at all.
        let a = call("json_get", "][");
        assert!(a.contains("not JSON"), "{a}");
        // Numbers and bools are stringified, so `[5]` works where a
        // plugin would otherwise have to learn that Tcl arguments are
        // all strings.
        let a = call("history", r#"["nosuchsession", 5]"#);
        assert!(a.contains("\"error\"") || a.contains("\"ok\""), "{a}");
        assert!(
            !a.contains("must be a number"),
            "a JSON number must pass: {a}"
        );

        // NULL pointers, which a C plugin can pass by accident.
        unsafe {
            let out = host_call(std::ptr::null(), std::ptr::null());
            assert!(!out.is_null());
            let s = CStr::from_ptr(out).to_string_lossy().into_owned();
            host_free_string(out);
            assert!(s.contains("null op name"), "{s}");
        }
        // A NULL args pointer alone means "no arguments".
        unsafe {
            let op = CString::new("tasks").expect("op");
            let out = host_call(op.as_ptr(), std::ptr::null());
            assert!(!out.is_null());
            let s = CStr::from_ptr(out).to_string_lossy().into_owned();
            host_free_string(out);
            assert!(s.contains("\"ok\""), "a NULL args pointer means []: {s}");
        }
        // Freeing NULL is a no-op, not a crash.
        unsafe { host_free_string(std::ptr::null_mut()) };
    }
}
