//! In-process modules: Rust dylibs the gateway loads, supervises and
//! unloads (PLAN-plugins-installer.md M2/M3, phase R3).
//!
//! WHY A MODULE
//!
//! A process plugin (pluginhost.rs) is a guest: it asks over JSON-RPC
//! and the gateway decides. That isolation is right for atproto, but a
//! PLATFORM cannot live behind it — IRC's ask, ACL and lockdown verbs
//! need the dispatcher that holds the per-chat lock, in-process (the
//! wall Phase 4 hit). A module is the opposite trade: a Rust dylib
//! linked to the SAME `libveles_runtime.so` the gateway runs, calling
//! core's ordinary API directly, with the HOST owning every resource so
//! cleanup is enforced, not left to the module's goodwill.
//!
//! WHAT THE HOST OWNS (M3)
//!
//! - **The runtime.** Each instance runs on a tokio runtime the host
//!   builds: 1 worker, named threads, the owner tag set in
//!   `on_thread_start`, a live-thread count in `on_thread_stop`. The
//!   module's `start(cx)` runs there, so a raw `tokio::spawn` in module
//!   code lands on that runtime too and dies with it.
//! - **The gate.** Every call INTO module code (start, stop) goes
//!   through the instance's [`Gate`]: it counts calls in flight and is
//!   closed at reload, so a late call is answered "reloading" instead of
//!   running against a half-torn-down instance.
//! - **The library.** The `Arc<libloading::Library>` lives in the
//!   instance entry; registrations hold only their owner tag (owned.rs
//!   says why: the registry entry must not keep the module mapped).
//! - **Teardown, in this order:** close the gate → `stop(reason)` with
//!   a deadline → purge every registry entry and slot owned by the
//!   instance → wait for in-flight calls to drain, bounded →
//!   `runtime.shutdown_timeout(deadline)` → quiescence check (no
//!   instance thread left, nothing in flight, the library's Arc count
//!   back to the loader's own, no tracing callsite of the module's own
//!   registered) → `dlclose`. A failed check keeps the library mapped,
//!   marked "code kept mapped: <why>" — never unmapped live code. Only
//!   a FAULT (a thread, a call, a reference that outlived the instance)
//!   is the instance's last error; a library held by tracing-core's
//!   callsites alone is the normal end of every module that logs
//!   through its own `tracing` macros, and is logged, not reported.
//!
//! WHAT THE HOST CANNOT FORCE (M3, honest list)
//!
//! Raw `std::thread::spawn`, plugin code pointers stored into a
//! third-party crate's global (tracing-core's callsite list is the one
//! R1 found: a module that logged through its own `tracing` macros —
//! veles-irc does, everywhere — stays mapped), sync infinite loops,
//! `unsafe` corruption. True in C++ too. The mitigations: logging
//! through `module::log`, the quiescence check, and the kept-mapped
//! fallback, whose cost is the mapping until restart — and, for a task
//! that never yields, the CPU its thread keeps burning.
//!
//! LIFECYCLE
//!
//! Mirrors the process tier's: discover → describe (from the FILE —
//! `module::inspect`/`check` before any dlopen, then the manifest from
//! the loaded static) → supervise with backoff → reload/stop/start →
//! the lease gate and standby routes behave the same. States are the
//! same names (`starting`, `running`, `backoff`, `stopped`, `standby`,
//! `refused`) plus `kept-mapped` on a library that could not unmap.
//! Every load, reload, fault and fault-kept-mapped is noted: nothing
//! ships silently (a library pinned by callsites alone is logged).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::bus::GatewayBus;
use crate::lockutil::LockOk;
use crate::module;
use crate::pluginhost::{self, LeaseGate};
use crate::rustplugins::{ModuleContext, VelesPlugin};
use veles_types::plugin::Manifest;

/// The module file name a plugin.toml of `kind = "module"` names:
/// `module.so`, or `module.dll` where the platform's modules are DLLs.
pub const DEFAULT_ENTRY: &str = if cfg!(windows) {
    "module.dll"
} else {
    "module.so"
};

/// Where installed modules are looked for, first match wins per name:
/// `$VELES_MODULE_PATH` (colon-separated), then `<data_dir>/modules`,
/// then `<exe>/../lib/veles/modules` (a bundle or a prefix install).
/// A separate variable from the process tier's `VELES_PLUGIN_PATH`: a
/// directory that holds both kinds can name both.
pub fn default_roots() -> Vec<PathBuf> {
    let mut v = crate::platform::split_search_path("VELES_MODULE_PATH");
    v.push(crate::config::data_dir().join("modules"));
    if let Some(prefix) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().and_then(Path::parent).map(Path::to_path_buf))
    {
        v.push(prefix.join("lib/veles/modules"));
    }
    v
}

/// One installed module: a directory with a `plugin.toml` of
/// `kind = "module"`.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub name: String,
    pub dir: PathBuf,
    /// The `.so` as installed (content-addressed under the module dir:
    /// `<dir>/<sha256>.so`, with `current` naming the newest). dlopen of
    /// a still-mapped path returns the OLD mapping, so a reload loads
    /// the new build from its own path and the old one unmaps when its
    /// last reference goes.
    pub so: PathBuf,
    /// The plugin.toml's `entry`, resolved against `dir`: the file a
    /// start or reload loads when the dir has no `current`.
    pub entry: PathBuf,
    pub description: String,
}

/// Every module under `roots`. A name found in an earlier root shadows
/// the same name later.
pub fn discover(roots: &[PathBuf]) -> Vec<Discovered> {
    let mut seen: BTreeMap<String, Discovered> = BTreeMap::new();
    for root in roots {
        let Ok(found) = crate::plugins::discover_all_in(root) else {
            continue;
        };
        for p in found {
            if p.manifest.kind != "module" || seen.contains_key(&p.manifest.name) {
                continue;
            }
            // The module file is the content-addressed `current` when the
            // dir was installed by `install_file`; otherwise the toml's
            // `entry` (a plain layout, a dev dir).
            let entry = PathBuf::from(&p.manifest.entry);
            let entry = if entry.is_absolute() {
                entry
            } else {
                p.dir.join(entry)
            };
            let so = current_so(&p.dir).unwrap_or_else(|| entry.clone());
            seen.insert(
                p.manifest.name.clone(),
                Discovered {
                    name: p.manifest.name.clone(),
                    dir: p.dir.clone(),
                    so,
                    entry,
                    description: p.manifest.description.clone(),
                },
            );
        }
    }
    seen.into_values().collect()
}

/// Install a built module file into `dir`, content-addressed: the file
/// lands at `<dir>/<sha256>.so` and `current` (a symlink, or a small
/// file with the name when symlinks are unavailable) points at it.
/// Returns the path to dlopen. A rebuild of the same bytes is a no-op.
pub fn install_file(dir: &Path, built: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let bytes = std::fs::read(built)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let ext = crate::platform::DYLIB_EXT;
    let target = dir.join(format!("{hash}.{ext}"));
    if !target.exists() {
        // Write beside the target and rename, so a reader never sees a
        // half-written module.
        let tmp = dir.join(format!(".{hash}.{ext}.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &target)?;
    }
    let cur = dir.join("current");
    // `current` is a symlink where the platform has them (both unix
    // targets do). Windows gets the one-line file carrying the name:
    // a symlink there needs a privilege or Developer Mode, and a
    // pointer file is read by `current_so` just the same.
    #[cfg(unix)]
    {
        // Swap atomically: a symlink beside it, then rename over
        // `current`. `remove_file` then `symlink` left a window with no
        // `current` at all, and a supervisor restarting the module in
        // that window fell back to its entry path (the one-time reload
        // `dlopen` failure in PENDING, 2026-10-11).
        let tmp = dir.join(format!(".current.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp)?;
        std::fs::rename(&tmp, &cur)?;
    }
    #[cfg(not(unix))]
    {
        let tmp = dir.join(format!(".current.{}.tmp", std::process::id()));
        std::fs::write(&tmp, format!("{hash}.{ext}"))?;
        crate::platform::replace_file(&tmp, &cur)?;
    }
    Ok(target)
}

/// The module file `dir`'s `current` names, if any.
pub fn current_so(dir: &Path) -> Option<PathBuf> {
    let cur = dir.join("current");
    if let Ok(link) = std::fs::read_link(&cur) {
        // `read_link` is the symlink's literal target. `install_file`
        // writes an ABSOLUTE link, but a module installed by hand or by
        // push-plugin.sh uses a RELATIVE one (`current -> <sha>.so`) —
        // which is also the only portable spelling across a bind-mount
        // remap (host `/srv/pool/...` vs container `/opt/data/...`).
        // Resolve a relative target against the module dir, never the
        // process CWD: a raw relative path made `inspect` read from the
        // gateway's CWD and fail ENOENT ("not a loadable module").
        return Some(if link.is_absolute() {
            link
        } else {
            dir.join(link)
        });
    }
    std::fs::read_to_string(&cur)
        .ok()
        .map(|s| dir.join(s.trim()))
        .filter(|p| p.exists())
}

/// Read a module file's manifest WITHOUT running its code: inspect +
/// check the file, dlopen, read the static, dlclose. The manifest's
/// `LazyLock` runs the module's manifest function — the only module
/// code that runs before supervision starts (M2: "describing a module
/// reads the static after dlopen; no plugin code runs" — the manifest
/// function is the declared exception, same as `--describe` for the
/// process tier).
pub fn describe(so: &Path) -> Result<Manifest, String> {
    let file = module::inspect(so).map_err(|r| r.to_string())?;
    module::check(&file).map_err(|r| r.to_string())?;
    // SAFETY: the file passed the key and symbol checks (same core,
    // same build). Loaded only to read the manifest; dropped before the
    // caller does anything with module code.
    let lib = unsafe { libloading::Library::new(so) }
        .map_err(|e| format!("dlopen {}: {e}", so.display()))?;
    let sym: libloading::Symbol<*const module::ExportedModule> =
        unsafe { lib.get(module::EXPORT_SYMBOL.as_bytes()) }.map_err(|e| {
            format!(
                "{}: no `{}` export: {e}",
                so.display(),
                module::EXPORT_SYMBOL
            )
        })?;
    let m = unsafe { &**sym }.manifest().clone();
    drop(lib);
    Ok(m)
}

/// The host's timings and limits. Shared shape with the process tier's
/// `HostOptions`, tuned for in-process: start and stop are deadlines on
/// module code, not on a child process.
#[derive(Debug, Clone)]
pub struct HostOptions {
    /// `start(cx)` must return within this.
    pub start_timeout: Duration,
    /// `stop(reason)` gets this long before teardown proceeds without it.
    pub stop_deadline: Duration,
    /// `runtime.shutdown_timeout` at teardown: the instance's tasks get
    /// this long to drop.
    pub shutdown_timeout: Duration,
    /// How long teardown waits for in-flight gate calls to drain.
    pub drain_timeout: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// An instance that ran this long before faulting resets the back-off.
    pub stable_after: Duration,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(30),
            stop_deadline: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(10),
            drain_timeout: Duration::from_secs(5),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
        }
    }
}

/// One instance, as `veles plugins status` shows it.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct InstanceStatus {
    pub plugin: String,
    pub instance: String,
    /// `starting`, `running`, `backoff`, `stopped`, `standby` (no lease),
    /// `refused` (never started: an inspect/check/describe problem),
    /// `kept-mapped` (unloaded, but the library could not be unmapped).
    pub state: String,
    pub version: String,
    pub restarts: u32,
    /// Unix seconds of the last state change.
    pub since: u64,
    pub last_error: Option<String>,
    pub lease: Option<String>,
    /// The path of the `.so` currently loaded, when running.
    pub so: Option<String>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An instance's dispatch gate (M3). Every call into module code goes
/// through it: `enter` counts a call in flight, a closed gate refuses
/// new calls ("reloading"). Teardown closes it, then waits for the
/// count to drain.
pub struct Gate {
    in_flight: AtomicUsize,
    closed: AtomicBool,
    /// Notified when `in_flight` reaches 0 after a close.
    drained: tokio::sync::Notify,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            drained: tokio::sync::Notify::new(),
        }
    }
}

impl Gate {
    /// Begin a call into module code. `Err` once the gate is closed:
    /// the instance is reloading.
    pub fn enter(self: &Arc<Self>) -> Result<GateGuard, &'static str> {
        if self.closed.load(Ordering::Acquire) {
            return Err("the instance is reloading");
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        // A close that landed between the check and the add must not be
        // missed: re-check now that the count covers us.
        if self.closed.load(Ordering::Acquire) {
            self.leave();
            return Err("the instance is reloading");
        }
        Ok(GateGuard {
            gate: Arc::clone(self),
        })
    }

    fn leave(&self) {
        if self.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drained.notify_waiters();
        }
    }

    /// Refuse new calls; existing ones finish.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        // An empty gate never gets a guard drop to notify it.
        if self.in_flight.load(Ordering::Acquire) == 0 {
            self.drained.notify_waiters();
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Wait for every in-flight call to finish, bounded.
    pub async fn drain(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if self.in_flight() == 0 {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::timeout_at(deadline, self.drained.notified())
                .await
                .ok();
        }
    }
}

/// An in-flight call; dropping it leaves the gate.
pub struct GateGuard {
    gate: Arc<Gate>,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.gate.leave();
    }
}

/// A host-built instance runtime (M3): 1 worker, named threads, the
/// owner tag set on every thread, a live-thread count the quiescence
/// check reads.
struct InstanceRuntime {
    rt: tokio::runtime::Runtime,
    threads: Arc<AtomicUsize>,
}

fn instance_runtime(owner: &'static str) -> std::io::Result<InstanceRuntime> {
    let threads = Arc::new(AtomicUsize::new(0));
    let (up, down) = (threads.clone(), threads.clone());
    // One worker. A task that never yields (a busy loop in module code)
    // stalls the instance's timers and I/O whatever the worker count:
    // tokio drives them from an idle worker parked on the driver, and a
    // second worker parked on its condvar does not take the driver
    // over. That is what the 2026-10-08 reload stalls were (the IRC
    // quit flush spun on an ended TLS stream, client.rs `flush_quit`),
    // not a single worker or a timer that needed ticking. A thread
    // still running at teardown is the quiescence check's first leg.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name(owner)
        .on_thread_start(move || {
            crate::lifecycle::set_thread_owner(owner);
            up.fetch_add(1, Ordering::SeqCst);
        })
        .on_thread_stop(move || {
            down.fetch_sub(1, Ordering::SeqCst);
        })
        .enable_all()
        .build()?;
    Ok(InstanceRuntime { rt, threads })
}

/// What a loaded module instance holds: the object, the library it came
/// from, the gate, the runtime. Teardown takes this BY VALUE (the watch
/// slot is cleared first): the runtime's `shutdown_timeout` consumes it,
/// and the library's Arc count is the quiescence check's third leg.
///
/// FIELD ORDER IS LOAD-BEARING: fields drop in declaration order, so
/// `plugin` (the module's object, whose vtable and drop glue live in the
/// library's mapping) is dropped BEFORE `lib` unmaps the code. The
/// reverse order was the R3 SIGSEGV: dropping the plugin object after
/// dlclose runs drop glue in unmapped memory.
struct Loaded {
    /// The constructed plugin object. Dropped FIRST.
    plugin: Arc<dyn VelesPlugin>,
    /// The library the object's code lives in. Dropped after it; only
    /// a teardown that cleared its check unmaps it ([`Mapping`]).
    lib: Mapping,
    /// The module's `veles_module` static. Points INTO `lib`'s mapping;
    /// never outlives it (we never hand the reference out).
    exported: &'static module::ExportedModule,
    /// The instance runtime. `Option` so teardown can TAKE it:
    /// `Runtime::shutdown_timeout` consumes the runtime, and a running
    /// instance's runtime is inside this struct.
    rt: Option<InstanceRuntime>,
    gate: Arc<Gate>,
    /// The path the library was loaded from (for maps checks and notes).
    so: PathBuf,
}

/// The module's library, which only a teardown whose quiescence check
/// cleared may unmap ([`Mapping::unmap`]). Any other drop LEAKS the
/// mapping: a `Loaded` dropped outside teardown, or a teardown future
/// dropped at one of its awaits. That last one happened: the gateway's
/// stop bounds each instance's Quit, the bound ran out first, the
/// gateway's runtime dropped the pending teardown, and its `Loaded`
/// dlclosed lang-lisp and ECL under the live engine thread. The
/// gateway died with SIGSEGV on its way out (2026-10-12).
struct Mapping(Option<Arc<libloading::Library>>);

impl Mapping {
    fn new(lib: libloading::Library) -> Self {
        Self(Some(Arc::new(lib)))
    }

    /// References to the library; the quiescence check wants only ours.
    fn strong_count(&self) -> usize {
        self.0.as_ref().map_or(0, Arc::strong_count)
    }

    /// Drop our reference, which dlcloses when it is the last one.
    fn unmap(mut self) {
        drop(self.0.take());
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if let Some(lib) = self.0.take() {
            std::mem::forget(lib);
        }
    }
}

impl Loaded {
    /// Inspect, check, dlopen, construct. The instance runtime is built
    /// here so a failure before it exists leaves nothing to tear down.
    fn load(so: &Path, owner: &'static str) -> Result<Loaded, String> {
        let file = module::inspect(so).map_err(|r| r.to_string())?;
        module::check(&file).map_err(|r| r.to_string())?;
        // SAFETY: the file passed the key and symbol checks. The module's
        // initialisers run here; its code runs only on `rt`'s threads
        // from here on (the gate).
        let lib = unsafe { libloading::Library::new(so) }
            .map_err(|e| format!("dlopen {}: {e}", so.display()))?;
        let sym: libloading::Symbol<*const module::ExportedModule> =
            unsafe { lib.get(module::EXPORT_SYMBOL.as_bytes()) }.map_err(|e| {
                format!(
                    "{}: no `{}` export: {e}",
                    so.display(),
                    module::EXPORT_SYMBOL
                )
            })?;
        // The static lives in the library's mapping; the library outlives
        // the reference because Loaded owns both and drops the reference's
        // usability with the lib (we never hand the reference out).
        let exported: &'static module::ExportedModule = unsafe { &**sym };
        let plugin = exported.construct();
        let rt = instance_runtime(owner).map_err(|e| format!("instance runtime: {e}"))?;
        Ok(Loaded {
            lib: Mapping::new(lib),
            exported,
            plugin,
            rt: Some(rt),
            gate: Arc::new(Gate::default()),
            so: so.to_path_buf(),
        })
    }

    /// The instance runtime's handle (for spawning `start`/`stop` onto).
    /// Panics after teardown took the runtime — a caller that can run
    /// then has a bug the supervisor's sequencing prevents.
    fn handle(&self) -> tokio::runtime::Handle {
        self.rt
            .as_ref()
            .expect("the runtime lives until teardown")
            .rt
            .handle()
            .clone()
    }

    /// Spawn `fut` on the instance runtime.
    fn spawn<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle().spawn(fut)
    }
}

enum Ctl {
    Reload(oneshot::Sender<String>),
    Stop(oneshot::Sender<String>),
    Start(oneshot::Sender<String>),
    /// The gateway is stopping: tear down and end the supervisor.
    Quit(oneshot::Sender<String>),
    /// Unload (R7): tear down, end the supervisor, and the host forgets
    /// the instance — gone from the listing until a `rescan`.
    Unload(oneshot::Sender<String>),
}

struct Instance {
    plugin: String,
    instance: String,
    /// `module:<plugin>[:<instance>]`, leaked ONCE here: the tag must
    /// outlive the threads that carry it, and every load of the
    /// instance reuses it.
    owner: &'static str,
    dir: PathBuf,
    /// Discovery's `entry`: what a load reads when `dir` has no `current`.
    entry: PathBuf,
    lease: Option<String>,
    manifest: Mutex<Manifest>,
    status: Mutex<InstanceStatus>,
    restarts: AtomicU32,
    want_stopped: AtomicBool,
    /// The live load; `None` while torn down. The supervisor swaps it.
    live: watch::Sender<Option<Arc<Loaded>>>,
    ctl_tx: mpsc::UnboundedSender<Ctl>,
    ctl_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Ctl>>,
}

impl Instance {
    fn label(&self) -> String {
        if self.instance.is_empty() {
            self.plugin.clone()
        } else {
            format!("{}:{}", self.plugin, self.instance)
        }
    }

    fn owner(&self) -> &'static str {
        self.owner
    }

    fn set_state(&self, state: &str, err: Option<String>) {
        let mut s = self.status.lock_ok();
        s.state = state.to_string();
        s.since = now_unix();
        if err.is_some() {
            s.last_error = err;
        }
        s.restarts = self.restarts.load(Ordering::Acquire);
        s.so = self
            .live
            .borrow()
            .as_ref()
            .map(|l| l.so.to_string_lossy().into_owned());
    }

    /// The instance's own directories under the host's roots, made
    /// private on first use (same shape as the process tier's).
    fn dirs(&self, host: &HostInner) -> veles_types::plugin::Dirs {
        let mut state = host.state_root.join(&self.plugin);
        let mut cache = host.cache_root.join(&self.plugin);
        if !self.instance.is_empty() {
            state = state.join(&self.instance);
            cache = cache.join(&self.instance);
        }
        for d in [&state, &cache] {
            let _ = crate::platform::private_dir(d);
        }
        veles_types::plugin::Dirs {
            state: state.to_string_lossy().into_owned(),
            cache: cache.to_string_lossy().into_owned(),
        }
    }

    /// Take `m` as this instance's manifest, unless it changes what was
    /// settled when the instance was made: its name, the lease it holds,
    /// the platforms it routes. Those a running gateway cannot move.
    fn adopt(&self, m: Manifest) -> Result<(), String> {
        if m.name != self.plugin {
            return Err(format!(
                "it says it is {:?}; expected {:?}",
                m.name, self.plugin
            ));
        }
        let restart = "the gateway takes that when it starts again";
        let lease = pluginhost::lease_key(&m, &self.instance)?;
        if lease != self.lease {
            return Err(format!(
                "the version installed now holds the lease {}, not {}; {restart}",
                lease.as_deref().unwrap_or("(none)"),
                self.lease.as_deref().unwrap_or("(none)")
            ));
        }
        let version = m.version.clone();
        crate::services::declare(&m.name, m.services.clone(), m.uses.clone());
        *self.manifest.lock_ok() = m;
        self.status.lock_ok().version = version;
        Ok(())
    }
}

/// Purge every registry entry and slot `owner` registered (M3 step 3).
/// The full census from R2: every purge entry point, in one place, so a
/// reload removes exactly the instance's footprint. Returns the entries
/// removed, for the teardown note.
pub(crate) fn purge_owner_everywhere(owner: Option<&'static str>) -> usize {
    let Some(_) = owner else { return 0 };
    let mut n = 0;
    // The plugin objects first (their tools ride with them per
    // rustplugins' contract), then the registries they fed.
    n += crate::rustplugins::purge_owner_plugins(owner);
    n += crate::tools::purge_owner_tools(owner);
    n += crate::commands::purge_owner_commands(owner);
    n += crate::deferred::purge_owner_deferred(owner);
    n += crate::providers::purge_owner_drivers(owner);
    n += crate::cron::purge_owner_session_runners(owner);
    // A module surface that holds sessions (session mail presence).
    n += crate::presence::purge_owner(owner);
    n += crate::acl::purge_owner_self_gated(owner);
    n += crate::acl::purge_owner_tool_caps(owner);
    n += crate::rooms::purge_owner_thread_rooms(owner);
    n += crate::irc_tap::purge_owner_presences(owner);
    n += crate::askrelay::purge_owner_sink(owner) as usize;
    // The seams R5 opened for the slopdrop module: the Tcl backend, the
    // public-reply policy, the pending-decision sources, the module's
    // HTTP routes, the delivery observers, the secret names it declared.
    n += crate::tcleval::purge_owner_backend(owner) as usize;
    n += crate::tcleval::purge_owner_public_reply(owner) as usize;
    // The services it provides (2026-10-10; the trigger forwarder is
    // one): what it covered answers again.
    n += crate::services::purge_owner(owner);
    n += crate::syncask::purge_owner_sources(owner);
    n += crate::moduleroutes::purge_owner_routes(owner);
    n += crate::bus::purge_owner_delivery_observers(owner);
    n += crate::bus::purge_owner_public_feeds(owner);
    n += crate::secrets::purge_owner_platform_secrets(owner);
    n += crate::ask::purge_owner_fallback(owner) as usize;
    n += crate::desktop_tools::purge_owner_pane_target(owner) as usize;
    // The layer slots (R7): what the owner wrapped or replaced is in
    // effect again.
    n += crate::layers::purge_owner(owner);
    // The script tier (R9): a language host module's owner takes its
    // host entry and every script it served with it (each script's
    // own footprint is purged by its own owner inside).
    n += crate::scripthost::purge_owner_hosts(owner);
    n
}

/// The quiescence check (M3 step 6): may the library be unmapped?
#[derive(Debug, Clone, PartialEq)]
enum UnloadCheck {
    /// Threads 0, nothing in flight, the Arc count back to the loader's
    /// own: dlclose.
    Clear,
    /// The library stays mapped, with the reason: something of the
    /// instance outlived it (a fault). Never unmap live code.
    KeptMapped(String),
    /// The instance ended cleanly, but tracing-core still points into
    /// the library (its callsites): kept mapped too, and no fault —
    /// every module that logs through its own `tracing` macros ends
    /// this way (IRC on every reload), so it is not the instance's
    /// last error.
    Pinned(String),
}

/// The check's legs, separated so a unit test can drive each without a
/// `Loaded` (which needs the dylib):
///
/// - `threads`: the instance's threads still alive AFTER its runtime's
///   shutdown. A task that never yields keeps its worker past
///   `shutdown_timeout`, still running module code.
/// - `in_flight`: gate calls that have not returned.
/// - `lib_strong`: the library's strong count (the loader's own is 1).
/// - `callsites`: tracing callsites registered from inside the module's
///   mapping (R1 hazard 1, `module::callsites_within`): tracing-core
///   never forgets one, and the next `Dispatch::new` walks its list
///   into whatever is mapped there.
fn quiescence_legs(
    threads: usize,
    in_flight: usize,
    lib_strong: usize,
    callsites: usize,
) -> UnloadCheck {
    if threads != 0 {
        return UnloadCheck::KeptMapped(format!(
            "{threads} instance thread(s) still running after the runtime's shutdown"
        ));
    }
    if in_flight != 0 {
        return UnloadCheck::KeptMapped(format!("{in_flight} call(s) still in flight"));
    }
    // The loader holds ONE Arc of the library (in `loaded`). Any extra
    // strong count means module code (or something holding module code)
    // outlives the instance — unmapping would leave it dangling.
    if lib_strong > 1 {
        return UnloadCheck::KeptMapped(format!(
            "the library has {} live references, expected 1",
            lib_strong
        ));
    }
    if callsites != 0 {
        return UnloadCheck::Pinned(format!(
            "{callsites} tracing callsite(s) of its own stay registered with tracing-core"
        ));
    }
    UnloadCheck::Clear
}

/// Is `path` (canonical) mapped into this process? The unload proof.
/// "Cannot tell" counts as mapped: the safe side of the question.
fn mapped(path: &Path) -> bool {
    crate::platform::mapped_ranges(path).is_none_or(|r| !r.is_empty())
}

struct HostInner {
    bus: GatewayBus,
    /// The gateway's store, handed to every instance in its context
    /// (R4): a platform module parks it for its own reads (IRC's
    /// `!history`). `None` until `with_store` (a test harness that
    /// never wires one leaves it None, and a module must not guess).
    store: Mutex<Option<Arc<Mutex<crate::store::Store>>>>,
    /// The gateway's node id, reported in statuses and used by the lease
    /// gate's fence (the process tier's twin uses it the same way).
    #[allow(dead_code)]
    node_id: String,
    opts: HostOptions,
    config: Mutex<Value>,
    instances: Mutex<BTreeMap<String, Arc<Instance>>>,
    refused: Mutex<Vec<InstanceStatus>>,
    /// Set by [`ModuleHost::shutdown_all`]: no supervisor starts again.
    quitting: AtomicBool,
    state_root: PathBuf,
    cache_root: PathBuf,
    /// The roots `rescan` re-discovers under.
    roots: Mutex<Vec<PathBuf>>,
    enabled: Mutex<Vec<String>>,
    disabled: Mutex<Vec<String>>,
    gate: Mutex<Option<Arc<dyn LeaseGate>>>,
}

/// The in-process module host. Cheap to clone.
#[derive(Clone)]
pub struct ModuleHost {
    inner: Arc<HostInner>,
}

static GLOBAL: OnceLock<ModuleHost> = OnceLock::new();

/// Make `host` the process's module host (the gateway does this once).
pub fn install(host: ModuleHost) {
    let _ = GLOBAL.set(host);
}

/// The running gateway's module host, if there is one.
pub fn global() -> Option<&'static ModuleHost> {
    GLOBAL.get()
}

impl ModuleHost {
    pub fn new(bus: GatewayBus, node_id: &str, opts: HostOptions) -> Self {
        Self {
            inner: Arc::new(HostInner {
                bus,
                store: Mutex::new(None),
                node_id: node_id.to_string(),
                opts,
                config: Mutex::new(json!({})),
                instances: Mutex::new(BTreeMap::new()),
                refused: Mutex::new(Vec::new()),
                quitting: AtomicBool::new(false),
                state_root: crate::config::data_dir().join("module-state"),
                cache_root: crate::config::cache_dir().join("module-cache"),
                roots: Mutex::new(Vec::new()),
                enabled: Mutex::new(Vec::new()),
                disabled: Mutex::new(Vec::new()),
                gate: Mutex::new(None),
            }),
        }
    }

    /// Wire the gateway's store (R4): every instance's context carries
    /// it. Called once at gateway boot, before `start`, mirroring the
    /// process tier's `with_store`.
    pub fn with_store(self, store: Arc<Mutex<crate::store::Store>>) -> Self {
        *self.inner.store.lock_ok() = Some(store);
        self
    }

    /// Discover, describe and start every enabled module under `roots`.
    /// Returns the labels of the instances it started supervising.
    /// Nothing here can fail the gateway: a module that cannot start is
    /// logged, noted and listed as `refused`.
    pub async fn start(
        &self,
        roots: &[PathBuf],
        enabled: &[String],
        disabled: &[String],
        config: Value,
        gate: Arc<dyn LeaseGate>,
    ) -> Vec<String> {
        // The process's signal handlers are the gateway's, pinned before the
        // first module's code runs here: a runtime that boots on a module
        // thread saves and restores what it finds (lang-racket), and a
        // handler that went in during that window was put back to the
        // default (the VPS gateway ignored its stop, 2026-10-10).
        #[cfg(unix)]
        if let Err(e) = crate::lifecycle::pin_signal_handlers() {
            tracing::warn!("the gateway's signal handlers are not pinned: {e}");
        }
        *self.inner.config.lock_ok() = config.clone();
        *self.inner.roots.lock_ok() = roots.to_vec();
        *self.inner.enabled.lock_ok() = enabled.to_vec();
        *self.inner.disabled.lock_ok() = disabled.to_vec();
        *self.inner.gate.lock_ok() = Some(gate.clone());
        let mut started = Vec::new();
        for d in discover(roots) {
            match self.start_discovered(&d, enabled, disabled, &config, gate.clone()) {
                Some(labels) => started.extend(labels),
                None => continue,
            }
        }
        started
    }

    /// One discovered module: describe it, then supervise each instance.
    /// `None` when the module is not enabled or was refused.
    fn start_discovered(
        &self,
        d: &Discovered,
        enabled: &[String],
        disabled: &[String],
        config: &Value,
        gate: Arc<dyn LeaseGate>,
    ) -> Option<Vec<String>> {
        // This attempt's outcome replaces the last one's: a module fixed
        // and rescanned must not stay listed `refused`.
        self.inner.refused.lock_ok().retain(|r| r.plugin != d.name);
        if !crate::plugins::is_enabled(&d.name, enabled, disabled) {
            tracing::info!("module {}: installed, not enabled", d.name);
            return None;
        }
        let manifest = match describe(&d.so) {
            Ok(m) => m,
            Err(e) => {
                self.refuse(&d.name, "", e);
                return None;
            }
        };
        // The per-node switch (R6): a node whose `.env` turns the
        // platform off, or lacks what it needs, spawns no instance and
        // asks for no lease. Listed as `off` with the reason, not refused.
        if let Some(why) = pluginhost::node_switch_here(&manifest).err() {
            tracing::info!("module {}: {why}", d.name);
            self.inner.refused.lock_ok().push(InstanceStatus {
                plugin: d.name.clone(),
                instance: String::new(),
                state: "off".into(),
                version: manifest.version.clone(),
                restarts: 0,
                since: now_unix(),
                last_error: Some(why),
                lease: None,
                so: None,
            });
            return None;
        }
        if manifest.name != d.name {
            self.refuse(
                &d.name,
                "",
                format!(
                    "{} describes itself as {:?}, but its plugin.toml says {:?}",
                    d.so.display(),
                    manifest.name,
                    d.name
                ),
            );
            return None;
        }
        // What it provides and uses (the dependency view), declared
        // whether or not it gets its lease here.
        crate::services::declare(
            &manifest.name,
            manifest.services.clone(),
            manifest.uses.clone(),
        );
        let names = match pluginhost::instances_of(&manifest, config) {
            Ok(n) => n,
            Err(e) => {
                self.refuse(&d.name, "", e);
                return None;
            }
        };
        if names.is_empty() {
            tracing::info!("module {}: no instances configured", d.name);
        }
        let mut started = Vec::new();
        for inst in names {
            let lease = match pluginhost::lease_key(&manifest, &inst) {
                Ok(l) => l,
                Err(e) => {
                    self.refuse(&d.name, &inst, e);
                    continue;
                }
            };
            let label = self.spawn_instance(d, manifest.clone(), inst, lease, gate.clone());
            started.push(label);
        }
        Some(started)
    }

    fn refuse(&self, plugin: &str, instance: &str, why: String) {
        tracing::error!("module {plugin}: not started: {why}");
        crate::notes::record_plugin_note(&format!("module {plugin} not started"), &why);
        self.inner.refused.lock_ok().push(InstanceStatus {
            plugin: plugin.to_string(),
            instance: instance.to_string(),
            state: "refused".into(),
            version: String::new(),
            restarts: 0,
            since: now_unix(),
            last_error: Some(why),
            lease: None,
            so: None,
        });
    }

    fn spawn_instance(
        &self,
        d: &Discovered,
        manifest: Manifest,
        instance: String,
        lease: Option<String>,
        gate: Arc<dyn LeaseGate>,
    ) -> String {
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let (live_tx, _) = watch::channel(None);
        let tag = if instance.is_empty() {
            format!("module:{}", d.name)
        } else {
            format!("module:{}:{}", d.name, instance)
        };
        let inst = Arc::new(Instance {
            plugin: d.name.clone(),
            instance: instance.clone(),
            owner: Box::leak(tag.into_boxed_str()),
            dir: d.dir.clone(),
            entry: d.entry.clone(),
            lease,
            manifest: Mutex::new(manifest),
            status: Mutex::new(InstanceStatus {
                plugin: d.name.clone(),
                instance: instance.clone(),
                state: "starting".into(),
                version: String::new(),
                restarts: 0,
                since: now_unix(),
                last_error: None,
                lease: None,
                so: None,
            }),
            restarts: AtomicU32::new(0),
            want_stopped: AtomicBool::new(false),
            live: live_tx,
            ctl_tx,
            ctl_rx: tokio::sync::Mutex::new(ctl_rx),
        });
        inst.status.lock_ok().lease = inst.lease.clone();
        let label = inst.label();
        self.inner
            .instances
            .lock_ok()
            .insert(label.clone(), inst.clone());
        let host = self.inner.clone();
        let spawn_label = label.clone();
        crate::rt::spawn(async move {
            match &inst.lease {
                Some(key) => {
                    let what = format!("module {spawn_label}");
                    let inst2 = inst.clone();
                    let host2 = host.clone();
                    // Until the lease is granted — and again after it
                    // is lost (the guard drops the duty) — the instance
                    // is `standby`, not `starting`: the laptop listed
                    // Telegram as starting for as long as the VPS held
                    // the lease (R6's live round).
                    // Standing by is no error: `last_error` stays as
                    // it was (the module_host test reads it after a
                    // reload and it must be None).
                    tracing::info!(target: "veles_core::modulehost", "{what}: standby, waiting for lease {key}");
                    inst.set_state("standby", None);
                    let duty: pluginhost::Duty = Box::new(move || {
                        let inst3 = inst2.clone();
                        let host3 = host2.clone();
                        Box::pin(async move {
                            let _back = StandbyOnDrop(inst3.clone(), host3.clone());
                            supervise(inst3, host3).await
                        })
                    });
                    if let Err(e) = gate.gated(key, &what, duty).await {
                        inst.set_state("refused", Some(format!("the lease refused: {e}")));
                    }
                }
                None => {
                    let _ = supervise(inst, host).await;
                }
            }
        });
        label
    }

    /// Rediscover under the start roots and start anything new: a first
    /// install without a gateway restart. Instances already supervised
    /// are left alone (reload is the verb for a new build).
    pub async fn rescan(&self) -> Vec<String> {
        let roots = self.inner.roots.lock_ok().clone();
        let enabled = self.inner.enabled.lock_ok().clone();
        let disabled = self.inner.disabled.lock_ok().clone();
        let config = self.inner.config.lock_ok().clone();
        let Some(gate) = self.inner.gate.lock_ok().clone() else {
            return Vec::new();
        };
        let mut started = Vec::new();
        for d in discover(&roots) {
            // Skip any module whose every configured instance is already
            // supervised. (A module with NO configured instances has no
            // instance to key on; it is supervised as `Single` by
            // instances_of, so this check covers it too.)
            let already: bool = {
                let insts = self.inner.instances.lock_ok();
                insts
                    .keys()
                    .any(|k| k == &d.name || k.starts_with(&format!("{}:", d.name)))
            };
            if already {
                continue;
            }
            if let Some(labels) =
                self.start_discovered(&d, &enabled, &disabled, &config, gate.clone())
            {
                started.extend(labels);
            }
        }
        started
    }

    /// The instances' status, for `veles plugins status` and the API.
    pub fn status(&self) -> Vec<InstanceStatus> {
        let mut v: Vec<InstanceStatus> = self
            .inner
            .instances
            .lock_ok()
            .values()
            .map(|i| {
                let mut s = i.status.lock_ok().clone();
                s.restarts = i.restarts.load(Ordering::Acquire);
                s.so = i
                    .live
                    .borrow()
                    .as_ref()
                    .map(|l| l.so.to_string_lossy().into_owned());
                s
            })
            .collect();
        v.extend(self.inner.refused.lock_ok().iter().cloned());
        v
    }

    fn targets(&self, target: &str) -> Result<Vec<Arc<Instance>>, String> {
        let all = self.inner.instances.lock_ok();
        let (plugin, inst) = match target.split_once(':') {
            Some((p, i)) => (p, Some(i)),
            None => (target, None),
        };
        let v: Vec<Arc<Instance>> = all
            .values()
            .filter(|i| i.plugin == plugin && inst.is_none_or(|n| i.instance == n))
            .cloned()
            .collect();
        if v.is_empty() {
            let known: Vec<String> = all.keys().cloned().collect();
            return Err(format!(
                "no running module instance {target} (known: {})",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
        Ok(v)
    }

    async fn control(
        &self,
        target: &str,
        make: fn(oneshot::Sender<String>) -> Ctl,
    ) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for i in self.targets(target)? {
            let (tx, rx) = oneshot::channel();
            if i.ctl_tx.send(make(tx)).is_err() {
                out.push(format!("{}: its supervisor is gone", i.label()));
                continue;
            }
            let wait = self.inner.opts.shutdown_timeout
                + self.inner.opts.stop_deadline
                + Duration::from_secs(5);
            match tokio::time::timeout(wait, rx).await {
                Ok(Ok(msg)) => out.push(format!("{}: {msg}", i.label())),
                _ => out.push(format!(
                    "{}: not running on this node (state {}); the request waits until it is",
                    i.label(),
                    i.status.lock_ok().state
                )),
            }
        }
        Ok(out)
    }

    /// Rebuild-and-reload: tear down the instance, load the file at its
    /// path now (content-addressed: a rebuilt module installed under the
    /// same dir is found at its new hash), start it.
    pub async fn reload(&self, target: &str) -> Result<Vec<String>, String> {
        self.control(target, Ctl::Reload).await
    }

    /// Tear down the instance(s); they stay stopped until `start`.
    pub async fn stop(&self, target: &str) -> Result<Vec<String>, String> {
        self.control(target, Ctl::Stop).await
    }

    /// Start what `stop` stopped.
    pub async fn start_again(&self, target: &str) -> Result<Vec<String>, String> {
        self.control(target, Ctl::Start).await
    }

    /// Tear the instance(s) down and forget them (R7): the same M3
    /// teardown as a stop, then the host drops them from its listing.
    /// A `rescan` finds the install again; `stop` is the verb that
    /// keeps the row.
    pub async fn unload(&self, target: &str) -> Result<Vec<String>, String> {
        let labels: Vec<String> = self.targets(target)?.iter().map(|i| i.label()).collect();
        let out = self.control(target, Ctl::Unload).await?;
        self.forget(&labels);
        Ok(out)
    }

    fn forget(&self, labels: &[String]) {
        let mut map = self.inner.instances.lock_ok();
        for l in labels {
            map.remove(l);
        }
    }

    /// File `action` (`reload`, `stop`, `start`, `unload`) for `target`
    /// and return at once with the instances it went to (the
    /// chat-command path).
    pub fn request(&self, target: &str, action: &str) -> Result<Vec<String>, String> {
        let make: fn(oneshot::Sender<String>) -> Ctl = match action {
            "reload" => Ctl::Reload,
            "stop" => Ctl::Stop,
            "start" => Ctl::Start,
            "unload" => Ctl::Unload,
            other => {
                return Err(format!(
                    "unknown action `{other}`: reload | stop | start | unload"
                ))
            }
        };
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for i in self.targets(target)? {
            let (tx, _) = oneshot::channel();
            if i.ctl_tx.send(make(tx)).is_ok() {
                out.push(i.label());
                gone.push(i.label());
            }
        }
        if action == "unload" {
            self.forget(&gone);
        }
        Ok(out)
    }

    /// The gateway is stopping: tear down every instance, all at once,
    /// and wait at most `within`.
    pub async fn shutdown_all(&self, within: Duration) {
        self.inner.quitting.store(true, Ordering::Release);
        let all: Vec<Arc<Instance>> = self.inner.instances.lock_ok().values().cloned().collect();
        let waits = all.into_iter().map(|i| {
            let (tx, rx) = oneshot::channel();
            // A supervisor holds the instance's ctl receiver for as long
            // as it runs. When the receiver is free, nothing supervises
            // the instance: it stands by for its lease, or the lease
            // refused it. Nobody would answer its Quit, and a supervisor
            // that starts after this sees `quitting` and returns. Every
            // laptop stop waited out the whole bound for Telegram
            // standing by on the VPS's lease (2026-10-12).
            let supervised = i.ctl_rx.try_lock().is_err();
            let sent = supervised && i.ctl_tx.send(Ctl::Quit(tx)).is_ok();
            async move {
                if sent {
                    let _ = tokio::time::timeout(within, rx).await;
                }
            }
        });
        futures::future::join_all(waits).await;
    }

    /// Is the module file at `path` currently mapped? Exposed for the
    /// unload smoke test.
    pub fn so_mapped(path: &Path) -> bool {
        mapped(path)
    }
}

/// Run the instance's work once: load, start, wait for a ctl or a fault,
/// tear down. Returns what ended it.
async fn run_once(
    inst: &Arc<Instance>,
    host: &Arc<HostInner>,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
) -> Ended {
    let label = inst.label();
    inst.set_state("starting", None);

    // A reload runs whatever file the module's dir names now — after an
    // update, a new build. Describe and adopt BEFORE loading: a version
    // that moved its lease or platforms is refused before its code runs.
    // Content-addressed installs resolve `current`; a plain dir's entry.
    let so = current_so(&inst.dir).unwrap_or_else(|| inst.entry.clone());
    let manifest = match describe(&so).and_then(|m| {
        let versioned = m.clone();
        inst.adopt(m).map(|_| versioned)
    }) {
        Ok(m) => m,
        Err(e) => return Ended::Exited(e),
    };

    let owner = inst.owner();
    // A fault recorded before this instance exists is its predecessor's:
    // a panic on the old instance's threads after it was told to stop
    // (2026-10-10: telegram's poll ran on into its runtime's shutdown).
    // Shown, not held against this one; left in, the poll below would
    // end every new instance at once.
    if let Some((loc, n)) = crate::module::take_faulted(owner) {
        let why = format!(
            "the previous instance panicked at {loc} ({n} time(s)) after it was ended; \
             not held against this one"
        );
        tracing::warn!("module {label}: {why}");
        crate::notes::record_plugin_note(&format!("module {label}: a late panic"), &why);
        inst.set_state("starting", Some(why));
    }
    // The watch slot holds the live load for status/diagnostics. It is
    // cleared by teardown BEFORE the runtime is consumed.
    let loaded = match Loaded::load(&so, owner) {
        Ok(l) => Arc::new(l),
        Err(e) => return Ended::Exited(e),
    };
    let version = loaded.exported.manifest().version.clone();
    inst.live.send_replace(Some(loaded.clone()));

    // Register the module's plugin object (hooks/tools/filters) under
    // the instance's owner, from an instance thread so the owner tag is
    // the instance's (M4: the owner is read from the thread, never
    // passed in).
    let plugin = loaded.plugin.clone();
    let reg = loaded
        .spawn(async move {
            crate::rustplugins::register_plugin(plugin);
        })
        .await;
    if let Err(e) = reg {
        let why = format!("registration failed: {e}");
        teardown(inst, host, loaded, "registration failed").await;
        return Ended::Exited(why);
    }

    // `start(cx)` on the instance runtime, through the gate, bounded.
    let cx = ModuleContext {
        plugin: inst.plugin.clone(),
        instance: inst.instance.clone(),
        config: pluginhost::sections_of(&manifest, &host.config.lock_ok()),
        dirs: inst.dirs(host),
        handle: loaded.handle(),
        bus: host.bus.clone(),
        store: host.store.lock_ok().clone(),
    };
    let plugin = loaded.plugin.clone();
    let gate = loaded.gate.clone();
    let started = loaded.spawn(async move {
        let _guard = match gate.enter() {
            Ok(g) => g,
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        };
        plugin.start(cx).await
    });
    let started = match tokio::time::timeout(host.opts.start_timeout, started).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(anyhow::anyhow!("start panicked: {e}")),
        Err(_) => Err(anyhow::anyhow!(
            "start did not return within {} s",
            host.opts.start_timeout.as_secs()
        )),
    };
    if let Err(e) = started {
        let why = format!("start failed: {e:#}");
        teardown(inst, host, loaded, "start failed").await;
        return Ended::Exited(why);
    }

    inst.set_state("running", None);
    tracing::info!("module {label}: running ({version})");
    crate::notes::record_plugin_note(
        &format!("module {label} started"),
        &format!("version {version}"),
    );

    // Wait: a ctl, or a module panic the M5 hook recorded for us. The
    // ctl arm re-awaits in a loop because a `Start` while running is a
    // no-op reply, not an end.
    loop {
        tokio::select! {
            c = ctl.recv() => {
                match c {
                    Some(Ctl::Start(ack)) => {
                        let _ = ack.send("already running".into());
                        continue;
                    }
                    Some(c) => {
                        let reason = match &c {
                            Ctl::Reload(_) => "reload",
                            Ctl::Quit(_) => "the gateway is stopping",
                            Ctl::Unload(_) => "unload",
                            _ => "stop",
                        };
                        let msg = teardown(inst, host, loaded, reason).await;
                        match c {
                            Ctl::Reload(ack) => { let _ = ack.send(msg); return Ended::Reload; }
                            Ctl::Stop(ack) => {
                                // The state says "stopped" BEFORE the
                                // caller hears so: `veles plugins stop`
                                // returns on this ack, and a status read
                                // right after it used to find "running"
                                // until the supervisor's next pass
                                // (a_stray_spawn_is_dropped_by_teardown,
                                // 2026-10-10 under load).
                                inst.want_stopped.store(true, Ordering::Release);
                                inst.set_state("stopped", None);
                                let _ = ack.send(msg);
                                return Ended::Stopped;
                            }
                            Ctl::Quit(ack) | Ctl::Unload(ack) => { let _ = ack.send(msg); return Ended::Quit; }
                            Ctl::Start(_) => unreachable!(),
                        }
                    }
                    None => {
                        teardown(inst, host, loaded, "the supervisor ended").await;
                        return Ended::Gone;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                // Poll the faulted registry for THIS instance: the M5
                // panic hook marks it when module code panics on an
                // instance thread. Taken, so the next instance starts
                // clean.
                if let Some((loc, n)) = crate::module::take_faulted(owner) {
                    let why = format!("module panicked at {loc} ({n} time(s))");
                    teardown(inst, host, loaded, "module panic").await;
                    return Ended::Exited(why);
                }
            }
        }
    }
}

/// Teardown, in M3's order. Takes the `Arc<Loaded>`: the watch slot is
/// cleared here, the runtime is consumed by `shutdown_timeout`, and the
/// library unmaps at the end of this call, only if the quiescence check
/// cleared. A caller that drops this future early leaves it mapped.
/// Returns the message the operator sees.
async fn teardown(
    inst: &Arc<Instance>,
    host: &Arc<HostInner>,
    loaded: Arc<Loaded>,
    reason: &str,
) -> String {
    let label = inst.label();
    let owner = inst.owner();

    // 1. Close the gate. New calls get "the instance is reloading".
    loaded.gate.close();

    // 2. stop(reason), a courtesy with a deadline. On the instance's
    //    runtime so module code runs where it always ran. The JoinHandle
    //    is dropped after the deadline: a stop that never returns must
    //    not park a handle whose task the runtime's shutdown would then
    //    walk into module code the teardown is about to unmap.
    let plugin = loaded.plugin.clone();
    let reason_owned = reason.to_string();
    let stopped = loaded.spawn(async move { plugin.stop(&reason_owned).await });
    // Said in the operator's answer too, not only the log: a stop that
    // runs out its deadline on every reload is a module bug to see.
    let late = if tokio::time::timeout(host.opts.stop_deadline, stopped)
        .await
        .is_err()
    {
        tracing::warn!(
            "module {label}: stop did not return within the deadline; tearing down anyway"
        );
        format!(
            "; its stop did not return within {} s",
            host.opts.stop_deadline.as_secs()
        )
    } else {
        String::new()
    };
    let reason = format!("{reason}{late}");
    let reason = reason.as_str();

    // 3. Purge every registry entry and slot the instance owns.
    let purged = purge_owner_everywhere(Some(owner));
    // Its bus routes, by name (the routes were registered under the
    // instance name by claim_routes — modules with platforms claim on
    // duty; R4 wires that when a module declares platforms).
    for p in &loaded.exported.manifest().platforms {
        host.bus.unregister_outbound_route(&p.key, &inst.instance);
    }

    // The instance no longer serves. Clear the slot BEFORE consuming the
    // runtime, so the only Arc<Loaded> left is ours.
    inst.live.send_replace(None);

    // 4. Drop every task on the instance runtime. `shutdown_timeout`
    //    CONSUMES the runtime and is not async; run it on a helper
    //    thread (we are on a core runtime thread, which it would join
    //    against itself). The helper also drops the runtime, so a task
    //    that outlived the timeout is dropped where the module is still
    //    mapped — its code is in the module this thread's caller unmaps
    //    only after `done_rx` answers.
    let mut taken = match Arc::try_unwrap(loaded) {
        Ok(l) => l,
        Err(_) => {
            // Something else holds the Loaded: a bug in our own
            // sequencing (the watch slot is cleared above). Keep the
            // library mapped rather than risk unmapping live code.
            let why = "the instance's load is still referenced".to_string();
            inst.set_state("kept-mapped", Some(why.clone()));
            crate::notes::record_plugin_note(
                &format!("module {label} kept mapped"),
                &format!("{reason}; {why}"),
            );
            return format!("unloaded; code kept mapped: {why}");
        }
    };
    let gate = taken.gate.clone();
    let so = taken.so.clone();
    let Some(instance_rt) = taken.rt.take() else {
        return "already torn down".into();
    };
    // The live-thread count outlives the runtime the helper consumes:
    // the quiescence check reads it after the shutdown.
    let threads = instance_rt.threads.clone();
    let shutdown_timeout = host.opts.shutdown_timeout;
    // 5. Wait for in-flight calls to drain BEFORE the runtime goes: a
    //    gate call holds module state the shutdown's task drops would
    //    touch. (The gate was closed in step 1, so nothing new starts.)
    let _ = gate.drain(host.opts.drain_timeout).await;

    let (done_tx, done_rx) = oneshot::channel();
    std::thread::spawn(move || {
        instance_rt.rt.shutdown_timeout(shutdown_timeout);
        let _ = done_tx.send(());
    });

    // Wait for the runtime to finish shutting down. `shutdown_timeout`'s
    // own bound governs how long tasks get; the join has NO extra timeout
    // because returning early leaves a task whose drop glue is module
    // code to be torn down by tokio's OwnedTasks walk AFTER this call
    // unmaps the library — the R3 harness SIGSEGV. The shutdown helper
    // thread is itself joined here, so no module-referencing work can
    // outlive the unmap.
    let _ = done_rx.await;
    // A shutdown that finished within its bound has JOINED every runtime
    // thread (tokio's blocking pool joins its workers when they all
    // report done), so the count below is exact: 0, or the threads a
    // task that never yielded still holds.

    // 6. Quiescence: no thread left, nothing in flight, the library's
    //    Arc count back to the loader's own (the one in `taken`), and no
    //    tracing callsite of the module's own registered.
    let lib_strong = taken.lib.strong_count();
    let canonical = std::fs::canonicalize(&so).unwrap_or_else(|_| so.clone());
    // The library's mapped ranges, for the callsite count. A platform
    // that cannot say (no /proc, a loader that refuses the question)
    // gives `None`, and the answer is then "keep it mapped": an empty
    // list here would read as "no callsites" and free a library that
    // tracing-core still points into — the crash this check exists to
    // prevent.
    let check = match crate::platform::mapped_ranges(&canonical) {
        Some(ranges) => quiescence_legs(
            threads.load(Ordering::SeqCst),
            gate.in_flight(),
            lib_strong,
            module::callsites_within(&ranges),
        ),
        None => UnloadCheck::KeptMapped(
            "this platform cannot tell where the library is mapped".to_string(),
        ),
    };

    // 7. dlclose, or keep mapped with the reason. The plugin object
    //    drops first (its drop glue is module code); then the Clear arm
    //    unmaps, and the others drop the mapping, which keeps it.
    let Loaded { plugin, lib, .. } = taken;
    drop(plugin);
    match check {
        UnloadCheck::Clear => {
            tracing::info!(
                "module {label}: torn down ({reason}; {purged} registration(s) purged; {})",
                so.display()
            );
            crate::notes::record_plugin_note(
                &format!("module {label} unloaded"),
                &format!("{reason}; {purged} registration(s) purged"),
            );
            lib.unmap();
            format!("unloaded ({reason}; {purged} registration(s) purged)")
        }
        UnloadCheck::KeptMapped(why) => {
            inst.set_state("kept-mapped", Some(why.clone()));
            tracing::warn!("module {label}: unloaded; code kept mapped: {why}");
            crate::notes::record_plugin_note(
                &format!("module {label} kept mapped"),
                &format!("{reason}; {why}; a bounded leak until the gateway restarts"),
            );
            // The module is still unloaded (gate closed, runtime down,
            // registrations purged); only its CODE stays mapped.
            drop(lib);
            format!("unloaded ({reason}; {purged} registration(s) purged); code kept mapped: {why}")
        }
        UnloadCheck::Pinned(why) => {
            // Mapped for the same reason, minus the fault: no last
            // error and no note (it would be one per reload), a log line.
            inst.set_state("kept-mapped", None);
            tracing::info!("module {label}: unloaded; code kept mapped: {why}");
            drop(lib);
            format!("unloaded ({reason}; {purged} registration(s) purged); code kept mapped: {why}")
        }
    }
}

/// How `run_once` ended, for the supervisor's backoff decision.
enum Ended {
    /// A reload: restart at once, backoff reset.
    Reload,
    /// Stopped on request: stay stopped until started.
    Stopped,
    /// The gateway is stopping.
    Quit,
    /// The module failed (start error, panic, a describe/adopt refusal).
    Exited(String),
    /// The supervisor's channel closed.
    Gone,
}

/// Held by the lease duty while it runs. The lease guard STOPS a duty
/// by dropping its future when the lease is lost, so this is where the
/// instance learns it is standing by again; a supervisor that ended on
/// its own (the gateway stopping, a stop, a refusal) is left as it is.
struct StandbyOnDrop(Arc<Instance>, Arc<HostInner>);

impl Drop for StandbyOnDrop {
    fn drop(&mut self) {
        if self.1.quitting.load(Ordering::Acquire) {
            return;
        }
        let state = self.0.status.lock_ok().state.clone();
        if matches!(state.as_str(), "stopped" | "refused" | "standby") {
            return;
        }
        tracing::info!(
            target: "veles_core::modulehost",
            "{}: the lease moved to another node; standing by for it again",
            self.0.label()
        );
        self.0.set_state("standby", None);
    }
}

async fn supervise(inst: Arc<Instance>, host: Arc<HostInner>) -> anyhow::Result<()> {
    if host.quitting.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut ctl = inst.ctl_rx.lock().await;
    let mut backoff = host.opts.backoff_base;
    loop {
        if host.quitting.load(Ordering::Acquire) {
            inst.set_state("stopped", None);
            return Ok(());
        }
        if inst.want_stopped.load(Ordering::Acquire) {
            inst.set_state("stopped", None);
            match ctl.recv().await {
                Some(Ctl::Quit(ack)) => {
                    let _ = ack.send("stopped".into());
                    return Ok(());
                }
                Some(Ctl::Unload(ack)) => {
                    let _ = ack.send("unloaded (was stopped)".into());
                    return Ok(());
                }
                Some(Ctl::Start(ack)) => {
                    inst.want_stopped.store(false, Ordering::Release);
                    let _ = ack.send("starting".into());
                    backoff = host.opts.backoff_base;
                }
                Some(Ctl::Stop(ack)) => {
                    let _ = ack.send("already stopped".into());
                    continue;
                }
                Some(Ctl::Reload(ack)) => {
                    let _ = ack.send("stopped; `start` it instead".into());
                    continue;
                }
                None => return Ok(()),
            }
        }
        let began = Instant::now();
        let ended = run_once(&inst, &host, &mut ctl).await;
        let label = inst.label();
        match ended {
            Ended::Gone => return Ok(()),
            Ended::Reload => {
                crate::notes::record_plugin_note(&format!("module {label} reloaded"), "");
                backoff = host.opts.backoff_base;
            }
            Ended::Stopped => {
                crate::notes::record_plugin_note(&format!("module {label} stopped"), "");
                inst.want_stopped.store(true, Ordering::Release);
            }
            Ended::Quit => {
                inst.set_state("stopped", None);
                return Ok(());
            }
            Ended::Exited(why) => {
                let n = inst.restarts.fetch_add(1, Ordering::AcqRel) + 1;
                if began.elapsed() >= host.opts.stable_after {
                    backoff = host.opts.backoff_base;
                }
                tracing::warn!(
                    "module {label}: {why}; restart {n} in {} ms",
                    backoff.as_millis()
                );
                crate::notes::record_plugin_note(
                    &format!("module {label} ended; restarting"),
                    &format!("{why} (restart {n}, in {} ms)", backoff.as_millis()),
                );
                inst.set_state("backoff", Some(why));
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    c = ctl.recv() => match c {
                        Some(Ctl::Stop(ack)) => {
                            inst.want_stopped.store(true, Ordering::Release);
                            let _ = ack.send("stopped".into());
                        }
                        Some(Ctl::Reload(ack)) => {
                            let _ = ack.send("restarting now".into());
                        }
                        Some(Ctl::Start(ack)) => {
                            let _ = ack.send("restarting now".into());
                        }
                        Some(Ctl::Quit(ack)) => {
                            inst.set_state("stopped", None);
                            let _ = ack.send("stopped".into());
                            return Ok(());
                        }
                        Some(Ctl::Unload(ack)) => {
                            inst.set_state("stopped", None);
                            let _ = ack.send("unloaded (was in backoff)".into());
                            return Ok(());
                        }
                        None => return Ok(()),
                    },
                }
                backoff = (backoff * 2).min(host.opts.backoff_max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate refuses after close, and drains.
    #[tokio::test]
    async fn a_closed_gate_refuses_and_drains() {
        let gate = Arc::new(Gate::default());
        let guard = gate.enter().expect("open");
        assert_eq!(gate.in_flight(), 1);
        gate.close();
        assert!(gate.enter().is_err());
        let g2 = gate.clone();
        let drained = crate::rt::spawn(async move { g2.drain(Duration::from_secs(5)).await });
        drop(guard);
        assert!(drained.await.expect("joined"));
    }

    /// Purge with `None` (core) removes nothing anywhere.
    #[test]
    fn purge_of_core_is_a_no_op() {
        assert_eq!(purge_owner_everywhere(None), 0);
    }

    /// The quiescence check reads threads, in-flight and Arc count.
    #[test]
    fn quiescence_reports_each_unquiet_leg() {
        // No runtime to build a real Loaded with in a unit test (it
        // needs the dylib), so exercise the check's logic through its
        // parts: the gate's count and the Arc count semantics.
        let gate = Arc::new(Gate::default());
        let _g = gate.enter().expect("open");
        assert_eq!(gate.in_flight(), 1, "one call in flight");
        drop(_g);
        assert_eq!(gate.in_flight(), 0);
    }

    /// The quiescence check's third leg: a library whose Arc is held
    /// beyond the loader's own is refused (the module's code is
    /// referenced somewhere the host does not own). Driven through the
    /// check itself.
    #[test]
    fn the_quiescence_check_refuses_a_pinned_library() {
        // Clear when every leg is quiet.
        assert_eq!(quiescence_legs(0, 0, 1, 0), UnloadCheck::Clear);
        // A pinned library (a second strong reference) is kept mapped.
        assert!(
            matches!(quiescence_legs(0, 0, 2, 0), UnloadCheck::KeptMapped(_)),
            "a pinned library must not unmap"
        );
        // And the other legs, each alone: a thread that outlived the
        // runtime's shutdown, a call in flight, a callsite of its own.
        assert!(matches!(
            quiescence_legs(1, 0, 1, 0),
            UnloadCheck::KeptMapped(_)
        ));
        assert!(matches!(
            quiescence_legs(0, 1, 1, 0),
            UnloadCheck::KeptMapped(_)
        ));
        // A callsite alone pins it too, but is no fault: not the
        // instance's last error.
        assert!(matches!(
            quiescence_legs(0, 0, 1, 1),
            UnloadCheck::Pinned(_)
        ));
        // A fault outranks the callsites it comes with.
        assert!(matches!(
            quiescence_legs(1, 0, 1, 1),
            UnloadCheck::KeptMapped(_)
        ));
    }

    #[test]
    fn installing_a_module_replaces_an_existing_current_pointer() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("module");
        let first = dir.join("first.so");
        let second = dir.join("second.so");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let selected_first = install_file(&dir, &first).unwrap();
        let selected_second = install_file(&dir, &second).unwrap();
        assert_ne!(selected_first, selected_second);
        assert_eq!(
            std::fs::read(current_so(&dir).expect("selected module")).unwrap(),
            b"second"
        );
        install_file(&dir, &first).unwrap();
        assert_eq!(
            std::fs::read(current_so(&dir).expect("rolled back module")).unwrap(),
            b"first"
        );
    }

    /// `current_so` resolves a RELATIVE `current` symlink against the
    /// module dir — never the process CWD. push-plugin.sh installs
    /// `current -> <sha>.so` (relative, the only spelling that survives
    /// the host↔container bind-mount remap), and before this fix the
    /// gateway's `inspect` read that raw target from its CWD and refused
    /// the module ENOENT. `install_file`'s absolute link still passes.
    #[cfg(unix)]
    #[test]
    fn current_so_joins_a_relative_symlink_to_the_module_dir() {
        let tmp = std::env::temp_dir().join(format!("veles-mh-{}", std::process::id()));
        let dir = tmp.join("irc");
        std::fs::create_dir_all(&dir).unwrap();
        let so = dir.join("abc123.so");
        std::fs::write(&so, b"elf").unwrap();
        // Relative symlink, the push-plugin.sh spelling.
        std::os::unix::fs::symlink("abc123.so", dir.join("current")).unwrap();
        let got = current_so(&dir).expect("resolves the relative link");
        assert_eq!(got, so.canonicalize().unwrap());
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// An absolute `current` symlink (install_file's spelling) is passed
    /// through untouched.
    #[cfg(unix)]
    #[test]
    fn current_so_passes_an_absolute_symlink_through() {
        let tmp = std::env::temp_dir().join(format!("veles-mh-abs-{}", std::process::id()));
        let dir = tmp.join("irc");
        std::fs::create_dir_all(&dir).unwrap();
        let so = dir.join("def456.so");
        std::fs::write(&so, b"elf").unwrap();
        std::os::unix::fs::symlink(&so, dir.join("current")).unwrap();
        let got = current_so(&dir).expect("resolves the absolute link");
        assert_eq!(got, so.canonicalize().unwrap());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
