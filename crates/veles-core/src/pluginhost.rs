//! Process plugins: executables the gateway spawns, supervises and talks
//! JSON-RPC to (PLAN-plugins-installer.md, D1–D5).
//!
//! WHY A PROCESS
//!
//! Every platform used to be a crate linked into the gateway, so a fix
//! to the IRC client restarted Telegram, Bluesky, cron and every running
//! turn with it. A `.so` cannot fix that: a Rust library with threads and
//! a tokio runtime cannot be safely unloaded, and a panic inside it is
//! the gateway's panic. A child process is the unit that can be rebuilt,
//! restarted and crash on its own.
//!
//! WHAT THE GATEWAY KEEPS
//!
//! The plugin is a guest. It asks; the gateway decides:
//!
//! - **Identity.** A plugin may vouch only for senders on a platform its
//!   manifest declares: an inbound `sender_id` of another platform is
//!   dropped to "unverified", and `sender_ctx.platform` is overwritten
//!   with the message's platform. Otherwise an IRC plugin could hand the
//!   gateway `telegram:<owner id>` and be the owner. Who a sender IS
//!   (`acl.resolve`) is answered from the gateway's live ACL.
//! - **Secrets.** A plugin gets a value only for a name its manifest
//!   declares, and its process starts with an EMPTY environment plus a
//!   short allowlist ([`CHILD_ENV`]) — the gateway's own environment
//!   carries every provider key.
//! - **Routing.** An inbound message's route is the instance it came
//!   from, whatever the plugin wrote there.
//!
//! LIFECYCLE
//!
//! One supervisor per instance: spawn, `initialize` (refused by name on
//! a protocol mismatch), run until the process ends or the operator
//! reloads or stops it. A crash restarts it with doubling back-off, reset
//! once it has run for a while. A reload asks the old process to shut
//! down, waits, then kills; outbound messages queue meanwhile (bounded,
//! with an age limit) and go out in order when the new process answers.
//! Under a cluster lease the supervisor only runs while this node holds
//! the instance's lease; losing it drops the supervisor, which kills the
//! child. The instance's platform routes are on the bus only while its
//! supervisor runs: a standby node must not answer "the adapter runs
//! here" (`GatewayBus::has_adapter`, which the owner's asks consult).
//!
//! Every start runs the binary installed now and adopts its
//! `--describe` before `initialize` (`Instance::adopt`): an update
//! takes effect at a reload, except a new lease or new platforms, which
//! were settled at spawn and wait for the gateway's next start.
//!
//! When the gateway stops, [`PluginHost::shutdown_all`] asks every
//! process to shut down (an IRC plugin sends its QUIT). When the gateway
//! dies without a word, the plugin's stdin closes and the SDK shuts it
//! down itself.
//!
//! Every start, crash, reload and stop parks a plugin note: nothing a
//! plugin does, and nothing done to one, is silent.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
pub use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::bus::{GatewayBus, InboundMessage, OutboundMessage};
use crate::lockutil::LockOk;
use veles_types::jsonrpc::{self, Handler, Options, Peer, RpcError};
use veles_types::plugin::{
    self as proto, method, ConfigGetParams, ConfigGetValue, Dirs, GrantParams, GrantResult,
    InitParams, Instances, Manifest, RecordPostsParams, ResolveParams, RoomParams, SecretParams,
    SecretValue, PROTOCOL,
};

/// The only environment variables a plugin process inherits.
pub const CHILD_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NIX_SSL_CERT_FILE",
];

/// Where installed process plugins are looked for, first match wins per
/// name: `$VELES_PLUGIN_PATH` (colon-separated), then
/// `<data_dir>/process-plugins`, then `<exe>/../lib/veles/plugins` (a
/// bundle or a prefix install). Not `<config_dir>/plugins`: that is the
/// Tcl plugins' git repo, snapshotted on every change.
pub fn default_roots() -> Vec<PathBuf> {
    let mut v = crate::platform::split_search_path("VELES_PLUGIN_PATH");
    v.push(crate::config::data_dir().join("process-plugins"));
    if let Some(prefix) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().and_then(Path::parent).map(Path::to_path_buf))
    {
        v.push(prefix.join("lib/veles/plugins"));
    }
    v
}

/// One installed process plugin: a directory with a `plugin.toml` of
/// `kind = "process"`.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub name: String,
    pub dir: PathBuf,
    pub exe: PathBuf,
    pub description: String,
}

/// Every process plugin under `roots`. A name found in an earlier root
/// shadows the same name later (so `VELES_PLUGIN_PATH` overrides an
/// installed copy).
pub fn discover(roots: &[PathBuf]) -> Vec<Discovered> {
    let mut seen: BTreeMap<String, Discovered> = BTreeMap::new();
    for root in roots {
        let Ok(found) = crate::plugins::discover_all_in(root) else {
            continue;
        };
        for p in found {
            if p.manifest.kind != "process" || seen.contains_key(&p.manifest.name) {
                continue;
            }
            let entry = PathBuf::from(&p.manifest.entry);
            let exe = if entry.is_absolute() {
                entry
            } else {
                p.dir.join(entry)
            };
            seen.insert(
                p.manifest.name.clone(),
                Discovered {
                    name: p.manifest.name.clone(),
                    dir: p.dir.clone(),
                    exe,
                    description: p.manifest.description.clone(),
                },
            );
        }
    }
    seen.into_values().collect()
}

fn child_command(exe: &Path) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(exe);
    c.env_clear();
    // The portable allowlist, plus what the platform's programs cannot
    // start without (`SystemRoot` and friends on Windows; nothing on
    // unix).
    for k in CHILD_ENV.iter().chain(crate::platform::CHILD_ENV_EXTRA) {
        if let Ok(v) = std::env::var(k) {
            c.env(k, v);
        }
    }
    c
}

/// Ask an executable what it is: `<exe> --describe`, no network, no
/// config, 10 seconds.
pub async fn describe(exe: &Path) -> Result<Manifest, String> {
    let mut c = child_command(exe);
    c.arg("--describe")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), c.output())
        .await
        .map_err(|_| format!("{} --describe: no answer in 10 s", exe.display()))?
        .map_err(|e| format!("{} --describe: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} --describe exited {}: {}",
            exe.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("{} --describe printed no manifest: {e}", exe.display()))
}

/// The subtree at a dotted path, `None` when any step is missing.
fn at_path<'a>(config: &'a Value, dotted: &str) -> Option<&'a Value> {
    dotted
        .split('.')
        .try_fold(config, |v, k| v.as_object().and_then(|o| o.get(k)))
}

/// The instance names a manifest asks for, given the config.
pub fn instances_of(m: &Manifest, config: &Value) -> Result<Vec<String>, String> {
    match &m.instances {
        Instances::Single => Ok(vec![String::new()]),
        Instances::PerEntry { array, name } => {
            let Some(arr) = at_path(config, array).and_then(Value::as_array) else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for (i, e) in arr.iter().enumerate() {
                let n = e.get(name).and_then(Value::as_str).ok_or_else(|| {
                    format!("{array}[{i}] has no string `{name}` to name its instance")
                })?;
                if !proto::valid_instance_name(n) {
                    return Err(format!(
                        "{array}[{i}].{name} = {n:?} cannot name an instance (letters, digits, \
                         `.`, `_`, `-`, at most 64)"
                    ));
                }
                if out.iter().any(|o| o == n) {
                    return Err(format!("{array}: two entries are named {n:?}"));
                }
                out.push(n.to_string());
            }
            Ok(out)
        }
    }
}

/// The per-NODE switch (R6): why a manifest's instances must NOT be
/// spawned on this node, if its `enabled_env` reads off or its
/// `required_env` is unset — decided BEFORE any lease is asked for, so
/// a node that must not run a platform never competes for it (the
/// laptop with `IRC_ENABLED=false` stood by under the IRC leases the
/// moment the IRC module was installed there, 2026-10-09). `resolve`
/// answers an env/dotenv name; `None` when unset.
pub fn node_switch(m: &Manifest, resolve: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    if let Some(var) = m.enabled_env.as_deref() {
        if let Some(v) = resolve(var) {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "false" | "0" | "no" | "off") {
                return Err(format!("off on this node ({var}={v})"));
            }
        }
    }
    if let Some(var) = m.required_env.as_deref() {
        if resolve(var).is_none_or(|v| v.trim().is_empty()) {
            return Err(format!("not configured on this node ({var} is not set)"));
        }
    }
    Ok(())
}

/// `node_switch` against this process's env and the config home's
/// dotenv.
pub fn node_switch_here(m: &Manifest) -> Result<(), String> {
    let home = crate::config::config_dir();
    // VELES_HOME-only, like the adapters' own token reads (pitfall 16):
    // a node must not inherit another install's switch or token.
    node_switch(m, |k| {
        crate::providers::resolve_env_or_dotenv_veles_only(k, &home)
    })
}

/// The config subtrees an instance is given.
pub fn sections_of(m: &Manifest, config: &Value) -> BTreeMap<String, Value> {
    m.config_sections
        .iter()
        .map(|s| {
            (
                s.clone(),
                at_path(config, s).cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// The lease an instance must hold, if the manifest names one. A key
/// must be an arbiter singleton (`singleton:…`).
pub fn lease_key(m: &Manifest, instance: &str) -> Result<Option<String>, String> {
    match &m.lease {
        None => Ok(None),
        Some(k) if k.starts_with("singleton:") => Ok(Some(k.replace("{instance}", instance))),
        Some(k) => Err(format!(
            "lease {k:?} is not an arbiter singleton (it must start with `singleton:`)"
        )),
    }
}

/// config.toml as JSON, for the sections plugins read (`{}` without a
/// file), with this node's `[node.overrides]` shadows applied.
pub fn raw_config_json() -> Value {
    let path = crate::config::config_dir().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return json!({});
    };
    match toml::from_str::<toml::Value>(&text) {
        Ok(mut t) => {
            // This node's `[node.overrides]` shadows hold here as they do
            // at the typed load (a plugin reads the VPS's Jetstream relay
            // while the shared file names the public URL).
            crate::config::apply_node_overrides_raw(&mut t);
            serde_json::to_value(t).unwrap_or_else(|_| json!({}))
        }
        Err(e) => {
            tracing::warn!("plugins: {} does not parse: {e}", path.display());
            json!({})
        }
    }
}

/// What a [`LeaseGate`] runs: called once per lease grant.
pub type Duty = Box<dyn FnMut() -> BoxFuture<'static, anyhow::Result<()>> + Send>;

/// Runs a duty only while this node may: the cluster's singleton lease.
#[async_trait]
pub trait LeaseGate: Send + Sync {
    /// Run `duty` (call it again after each lease regain) while this
    /// node holds `key`; when the lease is lost the running duty future
    /// is dropped. Returns when a duty returns. An `Err` without running
    /// the duty is a refusal: the instance is listed as refused, with
    /// the error as the reason.
    async fn gated(&self, key: &str, what: &str, duty: Duty) -> anyhow::Result<()>;
}

/// No cluster: every duty runs.
pub struct Ungated;

#[async_trait]
impl LeaseGate for Ungated {
    async fn gated(&self, _key: &str, _what: &str, mut duty: Duty) -> anyhow::Result<()> {
        duty().await
    }
}

/// The host's timings and limits.
#[derive(Debug, Clone)]
pub struct HostOptions {
    pub init_timeout: Duration,
    pub shutdown_grace: Duration,
    pub deliver_timeout: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// A process that ran this long before ending resets the back-off.
    pub stable_after: Duration,
    pub queue_cap: usize,
    pub queue_ttl: Duration,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            init_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(10),
            deliver_timeout: Duration::from_secs(120),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
            queue_cap: 1000,
            queue_ttl: Duration::from_secs(600),
        }
    }
}

/// One instance, as `veles plugins status` shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstanceStatus {
    pub plugin: String,
    pub instance: String,
    /// `starting`, `running`, `backoff`, `stopped`, `standby` (no lease),
    /// `refused` (never started: a describe or manifest problem).
    pub state: String,
    pub pid: Option<u32>,
    pub version: String,
    pub restarts: u32,
    /// Unix seconds of the last state change.
    pub since: u64,
    pub last_error: Option<String>,
    pub lease: Option<String>,
}

enum Ctl {
    Reload(oneshot::Sender<String>),
    Stop(oneshot::Sender<String>),
    Start(oneshot::Sender<String>),
    /// The gateway is stopping: shut down and end the supervisor.
    Quit(oneshot::Sender<String>),
    /// Unload (R7): shut down, end the supervisor, and the host forgets
    /// the instance (gone from the listing; a restart finds the install
    /// again — the process tier has no rescan).
    Unload(oneshot::Sender<String>),
}

/// How many of a plugin's own log lines are kept, per instance.
const LOG_KEEP: usize = 200;

struct Instance {
    plugin: String,
    instance: String,
    exe: PathBuf,
    lease: Option<String>,
    manifest: Mutex<Manifest>,
    status: Mutex<InstanceStatus>,
    restarts: AtomicU32,
    want_stopped: AtomicBool,
    peer: watch::Sender<Option<Peer>>,
    ctl_tx: mpsc::UnboundedSender<Ctl>,
    ctl_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Ctl>>,
    /// The instance's delivery queues, one per platform: put on the bus
    /// while the supervisor runs, taken off when it goes off duty.
    routes: Vec<(String, mpsc::UnboundedSender<OutboundMessage>)>,
    /// The last [`LOG_KEEP`] lines the process wrote to stderr.
    log: Mutex<VecDeque<String>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Instance {
    fn label(&self) -> String {
        if self.instance.is_empty() {
            self.plugin.clone()
        } else {
            format!("{}:{}", self.plugin, self.instance)
        }
    }

    fn set_state(&self, state: &str, pid: Option<u32>, err: Option<String>) {
        let mut s = self.status.lock_ok();
        s.state = state.to_string();
        s.pid = pid;
        s.since = now_unix();
        if err.is_some() {
            s.last_error = err;
        }
        s.restarts = self.restarts.load(Ordering::Acquire);
    }

    /// The instance's own directories under the host's roots
    /// ([`HostInner::state_root`]), made private on first use.
    fn dirs(&self, host: &HostInner) -> Dirs {
        let mut state = host.state_root.join(&self.plugin);
        let mut cache = host.cache_root.join(&self.plugin);
        if !self.instance.is_empty() {
            state = state.join(&self.instance);
            cache = cache.join(&self.instance);
        }
        for d in [&state, &cache] {
            let _ = std::fs::create_dir_all(d);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            }
        }
        Dirs {
            state: state.to_string_lossy().into_owned(),
            cache: cache.to_string_lossy().into_owned(),
        }
    }

    /// Claim this instance's platform routes on the bus (on duty).
    fn claim_routes(&self, bus: &GatewayBus) {
        for (platform, tx) in &self.routes {
            bus.register_outbound_route(platform, &self.instance, tx.clone());
        }
    }

    /// Give them back (off duty: lease lost, refused, the gateway stopping).
    fn release_routes(&self, bus: &GatewayBus) {
        for (platform, _) in &self.routes {
            bus.unregister_outbound_route(platform, &self.instance);
        }
    }

    /// Take `m` as this instance's manifest, unless it changes what was
    /// settled when the instance was spawned: its name and protocol, the
    /// lease it holds, the platforms it routes. Those a running gateway
    /// cannot move, and a version that moved them must not run under
    /// the old ones.
    fn adopt(&self, m: Manifest) -> Result<(), String> {
        if m.protocol != PROTOCOL || m.name != self.plugin {
            return Err(format!(
                "it says it is {} protocol {}; expected {} protocol {PROTOCOL}",
                m.name, m.protocol, self.plugin
            ));
        }
        let restart = "the gateway takes that when it starts again";
        let lease = lease_key(&m, &self.instance)?;
        if lease != self.lease {
            return Err(format!(
                "the version installed now holds the lease {}, not {}; {restart}",
                lease.as_deref().unwrap_or("(none)"),
                self.lease.as_deref().unwrap_or("(none)")
            ));
        }
        let mut now: Vec<&str> = m.platforms.iter().map(|p| p.key.as_str()).collect();
        let mut had: Vec<&str> = self.routes.iter().map(|(k, _)| k.as_str()).collect();
        now.sort_unstable();
        had.sort_unstable();
        if now != had {
            return Err(format!(
                "the version installed now serves the platforms [{}], not [{}]; {restart}",
                now.join(", "),
                had.join(", ")
            ));
        }
        let version = m.version.clone();
        crate::services::declare(&m.name, m.services.clone(), m.uses.clone());
        *self.manifest.lock_ok() = m;
        self.status.lock_ok().version = version;
        Ok(())
    }

    /// The owner its services are registered under while it runs.
    fn service_owner(&self) -> &'static str {
        crate::lifecycle::owner_tag(&format!("process:{}", self.label()))
    }

    /// Provide the manifest's services while the process runs. Each is
    /// answered by the plugin's own `request` handler, under the
    /// service's name, with the params as the caller sent them. An
    /// OVERRIDE POINT's request is an envelope instead,
    /// `{input, next, caller, session_id}` (PLAN-extension-api.md D2):
    /// the plugin answers the decision and may ask `service.next` with
    /// that `next` what the layer below would. Plain services keep their
    /// bare params, so protocol v1 is unchanged for them.
    fn provide_services(self: &Arc<Self>, timeout: Duration) {
        let owner = self.service_owner();
        crate::services::purge_owner(Some(owner));
        let decls = self.manifest.lock_ok().services.clone();
        for decl in decls {
            let inst = Arc::downgrade(self);
            let name = decl.name.clone();
            let handler = crate::services::handler(move |params, cx| {
                let inst = inst.clone();
                let name = name.clone();
                let params = if crate::services::is_point(&name) {
                    json!({
                        "input": params,
                        "next": cx.next,
                        "caller": cx.caller,
                        "session_id": cx.session_id,
                    })
                } else {
                    params
                };
                async move {
                    let inst = inst
                        .upgrade()
                        .ok_or_else(|| "the plugin is gone".to_string())?;
                    let peer = inst
                        .peer
                        .borrow()
                        .clone()
                        .ok_or_else(|| format!("{} is not running", inst.label()))?;
                    peer.call_timeout(&name, params, timeout)
                        .await
                        .map_err(|e| e.message)
                }
            });
            if let Err(e) = crate::services::register_for(owner, decl, handler) {
                tracing::warn!(
                    "plugin {}: a service it declares is refused: {e}",
                    self.label()
                );
            }
        }
    }

    /// The process is gone (or going): its services with it, and what
    /// they covered answers again.
    fn clear_peer(&self) {
        self.peer.send_replace(None);
        crate::services::purge_owner(Some(self.service_owner()));
    }

    fn declares_platform(&self, platform: &str) -> bool {
        self.manifest
            .lock_ok()
            .platforms
            .iter()
            .any(|p| p.key == platform)
    }
}

/// Clears the live peer and gives the routes back when a supervisor
/// ends for any reason, including being dropped by a lost lease:
/// deliveries must wait, not write into a dead pipe, and another node
/// now answers for the platform.
struct OffDuty(Arc<Instance>, GatewayBus);

impl Drop for OffDuty {
    fn drop(&mut self) {
        self.0.clear_peer();
        self.0.release_routes(&self.1);
        let state = self.0.status.lock_ok().state.clone();
        if state != "stopped" {
            self.0.set_state("standby", None, None);
        }
    }
}

struct HostInner {
    bus: GatewayBus,
    node_id: String,
    opts: HostOptions,
    config: Mutex<Value>,
    /// The gateway's store, for host methods that WRITE state a plugin may
    /// never hold a handle to (`pairing.grant_scoped`). None until
    /// [`PluginHost::with_store`] wires it (the gateway does, once); a
    /// grant request before that fails closed.
    store: Mutex<Option<Arc<Mutex<crate::store::Store>>>>,
    instances: Mutex<BTreeMap<String, Arc<Instance>>>,
    refused: Mutex<Vec<InstanceStatus>>,
    /// Set by [`PluginHost::shutdown_all`]: no supervisor starts again.
    quitting: AtomicBool,
    /// `<data>/plugin-state` and `<cache>/plugin-cache`, settled in
    /// [`PluginHost::new`]. A restart runs on whichever worker thread is
    /// free, where the directory lookup (a per-thread override, then the
    /// process env) can answer differently from the thread that made the
    /// host; an instance must come back to the state it left.
    state_root: PathBuf,
    cache_root: PathBuf,
}

/// The process-plugin host. Cheap to clone.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<HostInner>,
}

static GLOBAL: OnceLock<PluginHost> = OnceLock::new();

/// Make `host` the process's plugin host (the gateway does this once).
pub fn install(host: PluginHost) {
    let _ = GLOBAL.set(host);
}

/// The running gateway's plugin host, if there is one.
pub fn global() -> Option<&'static PluginHost> {
    GLOBAL.get()
}

impl PluginHost {
    pub fn new(bus: GatewayBus, node_id: &str, opts: HostOptions) -> Self {
        Self {
            inner: Arc::new(HostInner {
                bus,
                node_id: node_id.to_string(),
                opts,
                config: Mutex::new(json!({})),
                store: Mutex::new(None),
                instances: Mutex::new(BTreeMap::new()),
                refused: Mutex::new(Vec::new()),
                quitting: AtomicBool::new(false),
                state_root: crate::config::data_dir().join("plugin-state"),
                cache_root: crate::config::cache_dir().join("plugin-cache"),
            }),
        }
    }

    /// Wire the gateway's store, for the host methods that write state
    /// (`pairing.grant_scoped`). Called once at gateway start; cheap to
    /// clone the host afterwards. Returns self for chaining.
    pub fn with_store(self, store: Arc<Mutex<crate::store::Store>>) -> Self {
        *self.inner.store.lock_ok() = Some(store);
        self
    }

    /// Discover, describe and start every enabled process plugin under
    /// `roots`. Returns the labels of the instances it started
    /// supervising. Nothing here can fail the gateway: a plugin that
    /// cannot start is logged, noted and listed as `refused`.
    pub async fn start(
        &self,
        roots: &[PathBuf],
        enabled: &[String],
        disabled: &[String],
        config: Value,
        gate: Arc<dyn LeaseGate>,
    ) -> Vec<String> {
        *self.inner.config.lock_ok() = config.clone();
        let mut started = Vec::new();
        for d in discover(roots) {
            if !crate::plugins::is_enabled(&d.name, enabled, disabled) {
                tracing::info!("plugin {}: installed, not enabled", d.name);
                continue;
            }
            let manifest = match describe(&d.exe).await {
                Ok(m) => m,
                Err(e) => {
                    self.refuse(&d.name, "", e);
                    continue;
                }
            };
            if manifest.name != d.name {
                self.refuse(
                    &d.name,
                    "",
                    format!(
                        "{} describes itself as {:?}, but its plugin.toml says {:?}",
                        d.exe.display(),
                        manifest.name,
                        d.name
                    ),
                );
                continue;
            }
            // The per-node switch (R6), the same as the module host's: a
            // node whose `.env` turns the platform off, or lacks what it
            // needs, spawns no instance and asks for no lease.
            if let Err(why) = node_switch_here(&manifest) {
                tracing::info!("plugin {}: {why}", d.name);
                self.inner.refused.lock_ok().push(InstanceStatus {
                    plugin: d.name.clone(),
                    instance: String::new(),
                    state: "off".into(),
                    version: manifest.version.clone(),
                    restarts: 0,
                    since: now_unix(),
                    last_error: Some(why),
                    lease: None,
                    pid: None,
                });
                continue;
            }
            if manifest.protocol != PROTOCOL {
                self.refuse(
                    &d.name,
                    "",
                    format!(
                        "speaks plugin protocol {}; this gateway speaks {PROTOCOL}",
                        manifest.protocol
                    ),
                );
                continue;
            }
            // What it provides and uses, declared whether or not it runs
            // here (another node may hold its lease): the dependency view.
            crate::services::declare(
                &manifest.name,
                manifest.services.clone(),
                manifest.uses.clone(),
            );
            let names = match instances_of(&manifest, &config) {
                Ok(n) => n,
                Err(e) => {
                    self.refuse(&d.name, "", e);
                    continue;
                }
            };
            if names.is_empty() {
                tracing::info!("plugin {}: no instances configured", d.name);
            }
            for inst in names {
                let lease = match lease_key(&manifest, &inst) {
                    Ok(l) => l,
                    Err(e) => {
                        self.refuse(&d.name, &inst, e);
                        continue;
                    }
                };
                let label = self.spawn_instance(&d, manifest.clone(), inst, lease, gate.clone());
                started.push(label);
            }
        }
        started
    }

    fn refuse(&self, plugin: &str, instance: &str, why: String) {
        tracing::error!("plugin {plugin}: not started: {why}");
        crate::notes::record_plugin_note(&format!("plugin {plugin} not started"), &why);
        self.inner.refused.lock_ok().push(InstanceStatus {
            plugin: plugin.to_string(),
            instance: instance.to_string(),
            state: "refused".into(),
            pid: None,
            version: String::new(),
            restarts: 0,
            since: now_unix(),
            last_error: Some(why),
            lease: None,
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
        let (peer_tx, _) = watch::channel(None);
        // One delivery queue per platform this instance serves. It lives
        // as long as the gateway, across every restart of the process,
        // which is what lets a reload lose nothing. Its route goes on
        // the bus only while the instance is on duty (`supervise`).
        let mut routes = Vec::new();
        let mut queues = Vec::new();
        for p in &manifest.platforms {
            let (tx, rx) = mpsc::unbounded_channel();
            routes.push((p.key.clone(), tx));
            queues.push((p.key.clone(), rx));
        }
        let inst = Arc::new(Instance {
            plugin: d.name.clone(),
            instance: instance.clone(),
            exe: d.exe.clone(),
            lease: lease.clone(),
            status: Mutex::new(InstanceStatus {
                plugin: d.name.clone(),
                instance: instance.clone(),
                state: "starting".into(),
                pid: None,
                version: manifest.version.clone(),
                restarts: 0,
                since: now_unix(),
                last_error: None,
                lease: lease.clone(),
            }),
            manifest: Mutex::new(manifest.clone()),
            restarts: AtomicU32::new(0),
            want_stopped: AtomicBool::new(false),
            peer: peer_tx,
            ctl_tx,
            ctl_rx: tokio::sync::Mutex::new(ctl_rx),
            routes,
            log: Mutex::new(VecDeque::new()),
        });
        let label = inst.label();
        self.inner
            .instances
            .lock_ok()
            .insert(label.clone(), inst.clone());

        for (platform, rx) in queues {
            crate::rt::spawn(deliver_loop(inst.clone(), self.inner.clone(), platform, rx));
        }

        let host = self.inner.clone();
        let what = format!("plugin {label}");
        crate::rt::spawn(async move {
            let duty_inst = inst.clone();
            let duty: Duty = Box::new(move || {
                let (i, h) = (duty_inst.clone(), host.clone());
                Box::pin(async move { supervise(i, h).await })
            });
            let r = match inst.lease.clone() {
                Some(k) => {
                    inst.set_state("standby", None, None);
                    gate.gated(&k, &what, duty).await
                }
                None => Ungated.gated("", &what, duty).await,
            };
            if let Err(e) = r {
                tracing::error!("{what}: not running on this node: {e:#}");
                inst.set_state("refused", None, Some(format!("{e:#}")));
            }
        });
        label
    }

    /// Every instance, and every plugin that was refused.
    pub fn status(&self) -> Vec<InstanceStatus> {
        let mut v: Vec<InstanceStatus> = self
            .inner
            .instances
            .lock_ok()
            .values()
            .map(|i| {
                let mut s = i.status.lock_ok().clone();
                s.restarts = i.restarts.load(Ordering::Acquire);
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
                "no running plugin instance {target} (known: {})",
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
            // A standby instance (no lease here) has no supervisor
            // reading: say so instead of waiting forever.
            let wait = self.inner.opts.shutdown_grace + Duration::from_secs(5);
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

    /// Restart the process(es) of `target` (`name` or `name:instance`).
    pub async fn reload(&self, target: &str) -> Result<Vec<String>, String> {
        self.control(target, Ctl::Reload).await
    }

    /// Stop the process(es) of `target`; they stay stopped until `start`.
    pub async fn stop(&self, target: &str) -> Result<Vec<String>, String> {
        self.control(target, Ctl::Stop).await
    }

    /// Start what `stop` stopped.
    pub async fn start_again(&self, target: &str) -> Result<Vec<String>, String> {
        self.control(target, Ctl::Start).await
    }

    /// Shut the instance(s) down and forget them (R7): gone from the
    /// listing; the next gateway start discovers the install again.
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

    /// File `action` (`reload`, `stop`, `start` or `unload`) for `target` and
    /// return at once with the instances it went to. For a caller that
    /// cannot wait: the gateway answers chat commands from a sync
    /// dispatcher. `status` shows when it is done.
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
        for i in self.targets(target)? {
            // Nobody waits for the answer; the supervisor's ack goes
            // nowhere, which it already tolerates.
            let (tx, _) = oneshot::channel();
            if i.ctl_tx.send(make(tx)).is_ok() {
                out.push(i.label());
            }
        }
        if action == "unload" {
            self.forget(&out);
        }
        Ok(out)
    }

    /// The kept log lines of `target`'s instance(s), oldest first.
    pub fn logs(&self, target: &str) -> Result<Vec<(String, Vec<String>)>, String> {
        Ok(self
            .targets(target)?
            .into_iter()
            .map(|i| (i.label(), i.log.lock_ok().iter().cloned().collect()))
            .collect())
    }

    /// The gateway is stopping: ask every running instance to shut down,
    /// all at once, and wait at most `within`. Nothing restarts after
    /// this, not even on a lease regained in the meantime.
    pub async fn shutdown_all(&self, within: Duration) {
        self.inner.quitting.store(true, Ordering::Release);
        let all: Vec<Arc<Instance>> = self.inner.instances.lock_ok().values().cloned().collect();
        let waits = all.into_iter().map(|i| {
            let (tx, rx) = oneshot::channel();
            let sent = i.ctl_tx.send(Ctl::Quit(tx)).is_ok();
            async move {
                if sent {
                    // A standby instance has no supervisor reading: the
                    // timeout is its answer.
                    let _ = tokio::time::timeout(within, rx).await;
                }
            }
        });
        futures::future::join_all(waits).await;
    }

    /// Call a method on a running instance (a plugin's own service, a
    /// test hook).
    pub async fn call(
        &self,
        plugin: &str,
        instance: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let label = if instance.is_empty() {
            plugin.to_string()
        } else {
            format!("{plugin}:{instance}")
        };
        let inst = self
            .inner
            .instances
            .lock_ok()
            .get(&label)
            .cloned()
            .ok_or_else(|| RpcError::refused(format!("no plugin instance {label}")))?;
        let peer = inst
            .peer
            .borrow()
            .clone()
            .ok_or_else(|| RpcError::refused(format!("{label} is not running")))?;
        peer.call_timeout(method, params, self.inner.opts.deliver_timeout)
            .await
    }
}

enum Ended {
    Ctl(Ctl),
    Exited(String),
    Gone,
}

async fn supervise(inst: Arc<Instance>, host: Arc<HostInner>) -> anyhow::Result<()> {
    if host.quitting.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut ctl = inst.ctl_rx.lock().await;
    let _off = OffDuty(inst.clone(), host.bus.clone());
    // On duty: this node answers for the instance's platforms now.
    inst.claim_routes(&host.bus);
    let mut backoff = host.opts.backoff_base;
    let mut last_failure: Option<String> = None;
    loop {
        if host.quitting.load(Ordering::Acquire) {
            inst.set_state("stopped", None, None);
            return Ok(());
        }
        if inst.want_stopped.load(Ordering::Acquire) {
            inst.set_state("stopped", None, None);
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
                    last_failure = None;
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
            Ended::Ctl(Ctl::Reload(ack)) => {
                crate::notes::record_plugin_note(&format!("plugin {label} reloaded"), "");
                let _ = ack.send("reloading".into());
                backoff = host.opts.backoff_base;
                last_failure = None;
            }
            Ended::Ctl(Ctl::Stop(ack)) => {
                crate::notes::record_plugin_note(&format!("plugin {label} stopped"), "");
                inst.want_stopped.store(true, Ordering::Release);
                let _ = ack.send("stopped".into());
            }
            Ended::Ctl(Ctl::Start(ack)) => {
                let _ = ack.send("already running".into());
            }
            Ended::Ctl(Ctl::Quit(ack)) => {
                inst.set_state("stopped", None, None);
                let _ = ack.send("stopped".into());
                return Ok(());
            }
            Ended::Ctl(Ctl::Unload(ack)) => {
                inst.set_state("stopped", None, None);
                crate::notes::record_plugin_note(&format!("plugin {label} unloaded"), "");
                let _ = ack.send("unloaded".into());
                return Ok(());
            }
            Ended::Exited(why) => {
                let n = inst.restarts.fetch_add(1, Ordering::AcqRel) + 1;
                if began.elapsed() >= host.opts.stable_after {
                    backoff = host.opts.backoff_base;
                    last_failure = None;
                }
                // A plugin that fails the same way every time (an
                // unconfigured platform) is still retried, so fixing its
                // config brings it up, but it says so once: a note per
                // restart put up to 32 of them into the next reply.
                if last_failure.as_deref() == Some(why.as_str()) {
                    tracing::debug!(
                        "plugin {label}: {why}; restart {n} in {} ms",
                        backoff.as_millis()
                    );
                } else {
                    tracing::warn!(
                        "plugin {label}: {why}; restart {n} in {} ms (the same failure again is retried quietly)",
                        backoff.as_millis()
                    );
                    crate::notes::record_plugin_note(
                        &format!("plugin {label} ended; restarting"),
                        &format!("{why} (restart {n}, in {} ms)", backoff.as_millis()),
                    );
                    last_failure = Some(why.clone());
                }
                inst.set_state("backoff", None, Some(why));
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
                            inst.set_state("stopped", None, None);
                            let _ = ack.send("stopped".into());
                            return Ok(());
                        }
                        Some(Ctl::Unload(ack)) => {
                            inst.set_state("stopped", None, None);
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

/// One process, from spawn to its end.
async fn run_once(
    inst: &Arc<Instance>,
    host: &Arc<HostInner>,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
) -> Ended {
    let label = inst.label();
    inst.set_state("starting", None, None);
    // A restart runs whatever binary is at the path now — after an
    // update, a new version. Ask it what it is before talking to it:
    // `initialize` carries the config sections, and `secret.get` checks
    // the secrets, of the manifest held here, and the version the
    // gateway started with gave a new section nothing on its first run.
    if let Err(e) = describe(&inst.exe).await.and_then(|m| inst.adopt(m)) {
        return Ended::Exited(e);
    }
    let dirs = inst.dirs(host);
    let mut cmd = child_command(&inst.exe);
    cmd.current_dir(&dirs.state)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Ended::Exited(format!("cannot run {}: {e}", inst.exe.display())),
    };
    let pid = child.id();
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill().await;
        return Ended::Exited("no pipes to the child".into());
    };

    // The plugin's log lines, filed under its name, and the last few
    // kept for `veles plugins logs`.
    {
        let label = label.clone();
        let inst = inst.clone();
        crate::rt::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line: String = line.chars().take(4000).collect();
                let (level, msg) = plugin_line_level(&line);
                match level {
                    tracing::Level::ERROR => {
                        tracing::error!(target: "veles_core::plugin", "{label}: {msg}")
                    }
                    tracing::Level::WARN => {
                        tracing::warn!(target: "veles_core::plugin", "{label}: {msg}")
                    }
                    tracing::Level::INFO => {
                        tracing::info!(target: "veles_core::plugin", "{label}: {msg}")
                    }
                    tracing::Level::DEBUG => {
                        tracing::debug!(target: "veles_core::plugin", "{label}: {msg}")
                    }
                    tracing::Level::TRACE => {
                        tracing::trace!(target: "veles_core::plugin", "{label}: {msg}")
                    }
                }
                let mut log = inst.log.lock_ok();
                if log.len() >= LOG_KEEP {
                    log.pop_front();
                }
                log.push_back(line);
            }
        });
    }

    let handler = Arc::new(HostHandler {
        inst: inst.clone(),
        host: host.clone(),
    });
    let (peer, mut done) = jsonrpc::connect(stdout, stdin, handler, Options::default());

    let config = host.config.lock_ok().clone();
    let init = InitParams {
        protocol: PROTOCOL,
        host_version: env!("CARGO_PKG_VERSION").to_string(),
        node_id: host.node_id.clone(),
        plugin: inst.plugin.clone(),
        instance: inst.instance.clone(),
        config: sections_of(&inst.manifest.lock_ok(), &config),
        dirs,
    };
    let answer = tokio::select! {
        a = peer.call_timeout(
            method::INITIALIZE,
            serde_json::to_value(&init).unwrap_or(Value::Null),
            host.opts.init_timeout,
        ) => a,
        // A process that dies during initialize answers nothing.
        _ = &mut done => Err(RpcError::new(RpcError::CLOSED, "the process ended during initialize")),
    };
    let manifest: Manifest = match answer.and_then(|v| {
        serde_json::from_value(v).map_err(|e| RpcError::internal(format!("not a manifest: {e}")))
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = child.kill().await;
            let status = child
                .wait()
                .await
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            return Ended::Exited(format!("initialize failed: {}{status}", e.message));
        }
    };
    let version = manifest.version.clone();
    if let Err(e) = inst.adopt(manifest) {
        let _ = child.kill().await;
        return Ended::Exited(format!("initialize answered: {e}"));
    }
    inst.set_state("running", pid, None);
    inst.peer.send_replace(Some(peer.clone()));
    inst.provide_services(host.opts.deliver_timeout);
    tracing::info!(
        "plugin {label}: running (pid {}, {version})",
        pid.unwrap_or(0)
    );
    crate::notes::record_plugin_note(
        &format!("plugin {label} started"),
        &format!("pid {}, version {version}", pid.unwrap_or(0)),
    );

    loop {
        tokio::select! {
            reason = &mut done => {
                inst.clear_peer();
                let reason = reason.map(|r| r.to_string()).unwrap_or_else(|e| e.to_string());
                let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                    Ok(Ok(s)) => s.to_string(),
                    _ => {
                        let _ = child.kill().await;
                        "killed: it closed its output but kept running".into()
                    }
                };
                return Ended::Exited(format!("the process ended ({status}; {reason})"));
            }
            c = ctl.recv() => {
                if let Some(Ctl::Start(ack)) = c {
                    let _ = ack.send("already running".into());
                    continue;
                }
                inst.clear_peer();
                let reason = match &c {
                    Some(Ctl::Reload(_)) => "reload",
                    Some(Ctl::Quit(_)) | None => "the gateway is stopping",
                    Some(Ctl::Unload(_)) => "unload",
                    Some(_) => "stop",
                };
                let _ = peer
                    .call_timeout(
                        method::SHUTDOWN,
                        json!({"reason": reason}),
                        host.opts.shutdown_grace,
                    )
                    .await;
                if tokio::time::timeout(host.opts.shutdown_grace, child.wait())
                    .await
                    .is_err()
                {
                    tracing::warn!("plugin {label}: did not exit after shutdown; killed");
                    let _ = child.kill().await;
                }
                return match c {
                    Some(c) => Ended::Ctl(c),
                    None => Ended::Gone,
                };
            }
        }
    }
}

/// Outbound messages for one (platform, instance), in order, across
/// every restart of the process.
async fn deliver_loop(
    inst: Arc<Instance>,
    host: Arc<HostInner>,
    platform: String,
    mut rx: mpsc::UnboundedReceiver<OutboundMessage>,
) {
    let label = inst.label();
    let mut q: VecDeque<(Instant, OutboundMessage)> = VecDeque::new();
    let mut peer_rx = inst.peer.subscribe();
    let cap = host.opts.queue_cap.max(1);
    let push = |q: &mut VecDeque<(Instant, OutboundMessage)>, m: OutboundMessage| {
        if q.len() >= cap {
            q.pop_front();
            tracing::warn!(
                "plugin {label}: {platform} queue full ({cap}); dropped its oldest message"
            );
        }
        q.push_back((Instant::now(), m));
    };
    loop {
        if q.is_empty() {
            match rx.recv().await {
                Some(m) => push(&mut q, m),
                None => return,
            }
        }
        while let Ok(m) = rx.try_recv() {
            push(&mut q, m);
        }
        let before = q.len();
        q.retain(|(at, _)| at.elapsed() <= host.opts.queue_ttl);
        if q.len() < before {
            let n = before - q.len();
            tracing::warn!(
                "plugin {label}: dropped {n} {platform} message(s) older than {} s, never delivered",
                host.opts.queue_ttl.as_secs()
            );
            crate::notes::record_plugin_note(
                &format!("plugin {label}: {n} message(s) never delivered"),
                "the plugin was not running for longer than the queue keeps messages",
            );
        }
        let live = peer_rx.borrow().clone().filter(|p| !p.is_closed());
        let Some(peer) = live else {
            tokio::select! {
                r = peer_rx.changed() => if r.is_err() { return },
                m = rx.recv() => match m {
                    Some(m) => push(&mut q, m),
                    None => return,
                },
            }
            continue;
        };
        let Some((_, msg)) = q.pop_front() else {
            continue;
        };
        let v = match serde_json::to_value(&msg) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("plugin {label}: an outbound message would not serialize: {e}");
                continue;
            }
        };
        match peer
            .call_timeout(method::DELIVER, v, host.opts.deliver_timeout)
            .await
        {
            Ok(r) => {
                let r: proto::Receipt = serde_json::from_value(r).unwrap_or_default();
                tracing::debug!("plugin {label}: delivered to {} ({:?})", msg.chat_id, r.ids);
                // Whoever stamped `meta` on the message files the receipt
                // (R5: the slopdrop module's ledger records its post ids
                // and the hosted copy's link/blob ids). The host only
                // relays: the outbound meta, the ids, the receipt's meta.
                if !r.ids.is_empty() {
                    crate::bus::notify_delivered(&msg.meta, &r.ids, &r.meta);
                }
            }
            // Written, and then the process went away: it may or may not
            // have posted it. Not retried — a duplicate post is worse
            // than a lost one, and the log says which message it was.
            Err(e) if e.code == RpcError::CLOSED => tracing::warn!(
                "plugin {label}: the process ended while delivering to {}; outcome unknown, \
                 not retried",
                msg.chat_id
            ),
            Err(e) => tracing::warn!(
                "plugin {label}: delivery to {} failed: {}",
                msg.chat_id,
                e.message
            ),
        }
    }
}

/// What the gateway answers a plugin.
struct HostHandler {
    inst: Arc<Instance>,
    host: Arc<HostInner>,
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| RpcError::invalid_params(e.to_string()))
}

impl HostHandler {
    /// Does this plugin speak for `sender_id`'s platform?
    fn may_vouch_for(&self, sender_id: &str) -> bool {
        sender_id
            .split_once(':')
            .is_some_and(|(p, _)| self.inst.declares_platform(p))
    }

    fn note(&self, p: Value) {
        let n: proto::NoteParams = serde_json::from_value(p).unwrap_or_default();
        if n.summary.is_empty() {
            return;
        }
        crate::notes::record_plugin_note(
            &format!("plugin {}: {}", self.inst.label(), n.summary),
            &n.detail,
        );
    }

    fn inbound(&self, p: Value) {
        let label = self.inst.label();
        let mut msg: InboundMessage = match serde_json::from_value(p) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("plugin {label}: an inbound message did not parse: {e}");
                return;
            }
        };
        if !self.inst.declares_platform(&msg.platform) {
            tracing::warn!(
                "plugin {label}: dropped an inbound message for platform {:?}, which it does \
                 not declare",
                msg.platform
            );
            return;
        }
        if let Some(id) = msg.sender_id.as_deref() {
            if !self.may_vouch_for(id) {
                tracing::warn!(
                    "plugin {label}: vouched for {id:?}, outside its platforms; the sender is \
                     treated as unverified"
                );
                msg.sender_id = None;
            }
        }
        msg.sender_ctx.platform = Some(msg.platform.clone());
        msg.route = if self.inst.instance.is_empty() {
            None
        } else {
            Some(self.inst.instance.clone())
        };
        self.host.bus.send_inbound(msg);
    }

    /// `pairing.grant_scoped`: a scoped, TTL'd runtime grant for an
    /// identity on one of this plugin's own platforms. The gateway is the
    /// only writer of the ACL — a plugin never holds a store handle.
    /// Refused for an identity outside its platforms, and `owner` is never
    /// grantable (the adapter grants User to the operator's mutuals).
    fn grant(&self, p: Value) -> Result<Value, RpcError> {
        let label = self.inst.label();
        let gp: GrantParams = parse(p)?;
        if !self.may_vouch_for(&gp.identity) {
            return Err(RpcError::refused(format!(
                "{label} may grant only identities of its own platforms, not {}",
                gp.identity
            )));
        }
        if gp.role.eq_ignore_ascii_case("owner") {
            return Err(RpcError::refused("owner is not grantable"));
        }
        let store = self.host.store.lock_ok().clone().ok_or_else(|| {
            RpcError::refused(format!(
                "{label}: the gateway has no store wired for grants"
            ))
        })?;
        let g = crate::pairing::grant_scoped(
            &store,
            &gp.identity,
            &gp.role,
            gp.ttl_secs,
            gp.network,
            gp.channel,
        )
        .map_err(|e| RpcError::refused(format!("grant refused: {e:#}")))?;
        serde_json::to_value(GrantResult {
            granted: true,
            detail: format!(
                "{} granted {} (expires {})",
                g.identity, g.role, g.expires_at
            ),
        })
        .map_err(|e| RpcError::internal(e.to_string()))
    }

    /// `config.get`: the subtree at one dotted path, but only one the
    /// manifest declared in `config_sections` — a plugin reads only what
    /// it said it reads.
    fn config_get(&self, p: Value) -> Result<Value, RpcError> {
        let cp: ConfigGetParams = parse(p)?;
        let declared = self
            .inst
            .manifest
            .lock_ok()
            .config_sections
            .iter()
            .any(|s| s == &cp.path);
        if !declared {
            return Err(RpcError::refused(format!(
                "{} does not declare the config section {}; a plugin reads only what its \
                 manifest lists",
                self.inst.label(),
                cp.path
            )));
        }
        let cfg = self.host.config.lock_ok().clone();
        let value = at_path(&cfg, &cp.path).cloned().unwrap_or(Value::Null);
        serde_json::to_value(ConfigGetValue { value })
            .map_err(|e| RpcError::internal(e.to_string()))
    }

    /// `ledger.record_posts`: the plugin's delivery ids reported for a
    /// ledger entry after the fact (a late post). Relayed to the delivery
    /// observers like a receipt (R5): the module that owns the ledger
    /// records them.
    fn record_posts(&self, r: RecordPostsParams) {
        if r.entry_id.is_empty() || r.ids.is_empty() {
            return;
        }
        let mut meta = std::collections::BTreeMap::new();
        meta.insert(
            veles_types::bus::meta_keys::LEDGER_ENTRY.to_string(),
            r.entry_id.clone(),
        );
        crate::bus::notify_delivered(&meta, &r.ids, &std::collections::BTreeMap::new());
    }
}

#[async_trait]
impl Handler for HostHandler {
    async fn request(&self, _peer: &Peer, m: &str, p: Value) -> Result<Value, RpcError> {
        let label = self.inst.label();
        match m {
            method::SECRET => {
                let sp: SecretParams = parse(p)?;
                let declared = self
                    .inst
                    .manifest
                    .lock_ok()
                    .secrets
                    .iter()
                    .any(|s| s.name == sp.name);
                if !declared {
                    tracing::warn!("plugin {label}: asked for undeclared secret {}", sp.name);
                    return Err(RpcError::refused(format!(
                        "{label} does not declare the secret {}: a plugin is given only the \
                         names its manifest lists",
                        sp.name
                    )));
                }
                let value = crate::providers::resolve_env_or_dotenv_veles_only(
                    &sp.name,
                    &crate::config::config_dir(),
                );
                serde_json::to_value(SecretValue { value })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }
            method::RESOLVE => {
                let rp: ResolveParams = parse(p)?;
                if let Some(id) = rp.sender_id.as_deref() {
                    if !self.may_vouch_for(id) {
                        return Err(RpcError::refused(format!(
                            "{label} may resolve only senders of its own platforms, not {id}"
                        )));
                    }
                }
                let mut ctx = rp.sender_ctx;
                if ctx
                    .platform
                    .as_deref()
                    .is_some_and(|p| !self.inst.declares_platform(p))
                {
                    ctx.platform = None;
                }
                let acl = crate::acl::live_config();
                let who = crate::acl::Principal::resolve_ctx(rp.sender_id.as_deref(), &ctx, &acl);
                serde_json::to_value(who).map_err(|e| RpcError::internal(e.to_string()))
            }
            method::NOTE => {
                self.note(p);
                Ok(Value::Null)
            }
            method::GRANT => self.grant(p),
            method::CONFIG_GET => self.config_get(p),
            // Another plugin's (or core's) service, through the registry:
            // the provider's declared access decides, as for a script.
            method::SERVICE_CALL => {
                let sc: veles_types::plugin::ServiceCallParams = parse(p)?;
                crate::services::call_from_plugin(
                    &format!("process:{label}"),
                    &sc.service,
                    sc.params,
                    sc.session_id,
                )
                .await
                .map_err(RpcError::refused)
            }
            // What the layer below this plugin's own answers (an override
            // point's `next`): the token its request carried names it.
            method::SERVICE_NEXT => {
                let sn: veles_types::plugin::ServiceNextParams = parse(p)?;
                crate::services::call_next(&sn.token, sn.params)
                    .await
                    .map_err(RpcError::refused)
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }

    async fn notification(&self, _peer: &Peer, m: &str, p: Value) {
        match m {
            method::INBOUND => self.inbound(p),
            method::NOTE => self.note(p),
            method::ROOM => {
                let r: RoomParams = serde_json::from_value(p).unwrap_or_default();
                if !r.thread.is_empty() {
                    crate::rooms::register_thread_room(&r.thread, &r.handles);
                }
            }
            method::RECORD_POSTS => {
                let r: RecordPostsParams = serde_json::from_value(p).unwrap_or_default();
                self.record_posts(r);
            }
            other => tracing::debug!(
                "plugin {}: an unknown notification {other}",
                self.inst.label()
            ),
        }
    }
}

/// The level a plugin wrote a stderr line at, and the line without it.
///
/// The SDK logs `{LEVEL} {target}: {msg}` (tracing's plain format with no
/// time, the level right-aligned to five columns). The gateway files the
/// line at that level, so a plugin's WARN is a WARN in the journal and in
/// every level filter. A line not in that shape (a panic message, a stray
/// `eprintln!`) is unexpected on a plugin's stderr, so it is a WARN.
fn plugin_line_level(line: &str) -> (tracing::Level, &str) {
    use tracing::Level;
    let t = line.trim_start();
    for (name, level) in [
        ("ERROR", Level::ERROR),
        ("WARN", Level::WARN),
        ("INFO", Level::INFO),
        ("DEBUG", Level::DEBUG),
        ("TRACE", Level::TRACE),
    ] {
        if let Some(rest) = t.strip_prefix(name) {
            if rest.starts_with(' ') {
                return (level, rest.trim_start());
            }
        }
    }
    (Level::WARN, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin's WARN has to reach the journal as a WARN: filed at INFO,
    /// every level filter missed it (the live atproto poll warnings sat at
    /// INFO under `veles_core::plugin` until 2026-10-07). The SDK writes
    /// `{LEVEL} {target}: {msg}` with the level right-aligned to five
    /// columns; anything else on stderr (a panic message, a stray
    /// `eprintln!`) is unexpected, so it is a WARN too.
    #[test]
    fn a_plugin_line_keeps_its_level() {
        use tracing::Level;
        assert_eq!(
            plugin_line_level(" WARN veles_atproto::notify: poll failed"),
            (Level::WARN, "veles_atproto::notify: poll failed")
        );
        assert_eq!(
            plugin_line_level("ERROR x: boom"),
            (Level::ERROR, "x: boom")
        );
        assert_eq!(plugin_line_level(" INFO x: fine"), (Level::INFO, "x: fine"));
        assert_eq!(plugin_line_level("DEBUG x: d"), (Level::DEBUG, "x: d"));
        assert_eq!(plugin_line_level("TRACE x: t"), (Level::TRACE, "x: t"));
        assert_eq!(
            plugin_line_level("thread 'main' panicked at src/main.rs:1:1"),
            (Level::WARN, "thread 'main' panicked at src/main.rs:1:1")
        );
        // A word that merely starts like a level is not one.
        assert_eq!(
            plugin_line_level("INFORMATION overload"),
            (Level::WARN, "INFORMATION overload")
        );
    }

    fn manifest(instances: Instances, lease: Option<&str>) -> Manifest {
        Manifest {
            name: "x".into(),
            version: "1".into(),
            protocol: PROTOCOL,
            instances,
            lease: lease.map(str::to_string),
            config_sections: vec!["platforms.x".into(), "missing.section".into()],
            ..Default::default()
        }
    }

    /// The per-node switch: `enabled_env` off or `required_env` unset
    /// keeps a node from spawning the instances; anything else lets it.
    #[test]
    fn the_node_switch_reads_the_two_env_names() {
        let env = |pairs: &[(&str, &str)]| {
            let v: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| v.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        let mut m = manifest(Instances::Single, None);
        assert!(
            node_switch(&m, env(&[])).is_ok(),
            "no switch, no requirement"
        );
        m.enabled_env = Some("IRC_ENABLED".into());
        assert!(node_switch(&m, env(&[])).is_ok(), "unset is on");
        assert!(node_switch(&m, env(&[("IRC_ENABLED", "true")])).is_ok());
        for off in ["false", "0", "no", "OFF", " false "] {
            let e = node_switch(&m, env(&[("IRC_ENABLED", off)])).unwrap_err();
            assert!(e.contains("IRC_ENABLED="), "{off:?}: {e}");
        }
        m.enabled_env = None;
        m.required_env = Some("TELEGRAM_BOT_TOKEN".into());
        let e = node_switch(&m, env(&[])).unwrap_err();
        assert!(e.contains("TELEGRAM_BOT_TOKEN is not set"), "{e}");
        assert!(node_switch(&m, env(&[("TELEGRAM_BOT_TOKEN", "  ")])).is_err());
        assert!(node_switch(&m, env(&[("TELEGRAM_BOT_TOKEN", "123:abc")])).is_ok());
    }

    #[test]
    fn per_entry_instances_come_from_the_config() {
        let m = manifest(
            Instances::PerEntry {
                array: "platforms.x.nets".into(),
                name: "name".into(),
            },
            None,
        );
        let cfg = json!({"platforms": {"x": {"nets": [{"name": "a"}, {"name": "b.c"}]}}});
        assert_eq!(instances_of(&m, &cfg).unwrap(), vec!["a", "b.c"]);
        // No array configured: no instances, not an error.
        assert!(instances_of(&m, &json!({})).unwrap().is_empty());
        // A name that could escape its directory or its lease key.
        let bad = json!({"platforms": {"x": {"nets": [{"name": "../etc"}]}}});
        assert!(instances_of(&m, &bad).is_err());
        let dup = json!({"platforms": {"x": {"nets": [{"name": "a"}, {"name": "a"}]}}});
        assert!(instances_of(&m, &dup).unwrap_err().contains("two entries"));
        assert_eq!(
            instances_of(&manifest(Instances::Single, None), &json!({})).unwrap(),
            vec![""]
        );
    }

    #[test]
    fn an_instance_gets_exactly_its_declared_sections() {
        let m = manifest(Instances::Single, None);
        let cfg = json!({"platforms": {"x": {"k": 1}, "other": {"secret": 2}}, "acl": {}});
        let s = sections_of(&m, &cfg);
        assert_eq!(s.len(), 2);
        assert_eq!(s["platforms.x"], json!({"k": 1}));
        assert_eq!(s["missing.section"], Value::Null);
    }

    #[test]
    fn a_lease_must_be_an_arbiter_singleton() {
        let m = manifest(Instances::Single, Some("singleton:irc:{instance}"));
        assert_eq!(
            lease_key(&m, "synirc").unwrap().as_deref(),
            Some("singleton:irc:synirc")
        );
        let m = manifest(Instances::Single, Some("node:abc"));
        assert!(lease_key(&m, "").is_err());
        assert_eq!(
            lease_key(&manifest(Instances::Single, None), "").unwrap(),
            None
        );
    }
}
