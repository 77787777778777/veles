//! Node identity, capability probe, and cluster bootstrap (Plan B,
//! milestone B1, 2026-09-20).
//!
//! The pool reframe: laptop and vps are not "two instances" that call
//! each other 1:1 — they are compute backends in one pool, sharing one
//! logical identity (config, memory, sessions) served by whichever
//! workers are up. This module is the node's own answer to three
//! questions:
//!
//!   1. WHO am I   — a stable `NodeIdentity` persisted in
//!      `<config_dir>/node.toml` on first boot. The identity lives
//!      OUTSIDE config.toml on purpose: a config you can copy between
//!      machines must not carry the one value that tells them apart.
//!   2. WHAT can I do — a `NodeCaps` probe (GPU via `nvidia-smi`,
//!      uptime class, hostname, version) plus free-form `[node.caps]`
//!      overrides for what no probe can see.
//!   3. HOW do I join — a `Bootstrap` trait. B1 ships `static` (dial
//!      configured seeds); the trait is the seam Plan C's DHT slots
//!      behind without a config rewrite, which is the whole reason the
//!      config key is a NAME (`cluster.bootstrap`) and not a mechanism.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The persisted identity file name, inside `config_dir()`.
const NODE_FILE: &str = "node.toml";

/// A node's stable identity. `id` is a UUID minted once and persisted;
/// `name` is display-only and may track the hostname.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

impl NodeIdentity {
    /// Load the identity if this machine has one — WITHOUT minting. The
    /// read paths (`veles nodes list/show`, the `veles::nodes` bridge op)
    /// use this: a question about the registry must not write
    /// `node.toml` as a side effect.
    pub fn load(config_dir: &Path) -> Result<Option<Self>> {
        let path = config_dir.join(NODE_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let id: NodeIdentity =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        Ok(Some(id))
    }

    /// Load the identity, minting and persisting one on first boot.
    ///
    /// The write is atomic (temp file + rename): a gateway that crashes
    /// between "minted" and "saved" must not leave a truncated identity
    /// that the next boot reads as authoritative.
    pub fn load_or_create(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join(NODE_FILE);
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let id: NodeIdentity =
                toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
            return Ok(id);
        }
        let id = NodeIdentity {
            id: uuid::Uuid::new_v4().to_string(),
            name: hostname(),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(&id)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(id)
    }
}

/// The machine's hostname. Shared with profiles.rs's copy — kept local
/// (and cfg-gated the same way) rather than imported, because a node
/// identity must resolve even when no profile machinery has run.
pub fn hostname() -> String {
    crate::platform::hostname()
}

/// The GPU half of the capability probe. `None` means "no GPU *or*
/// nothing to probe with" — the two are deliberately not distinguished
/// at this layer (a node with no nvidia-smi and a node with no NVIDIA
/// card are equally unable to take GPU work, which is all placement
/// cares about at B1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GpuInfo {
    pub name: String,
    #[serde(default)]
    pub memory_total_mb: Option<u64>,
}

/// How long the GPU probe may take before it is abandoned.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ── load metrics (B4: least-loaded placement) ────────────────────────

/// The one-number load a scheduler can compare across nodes (B4).
///
/// UNIX-ish 1-minute load average normalized by CPU count, clamped to
/// 100: 0 = idle, 100 = at least fully saturated, comparable across
/// machines with different core counts. The reason to normalize BEFORE
/// it enters the registry: a RAW loadavg would make the laptop (16
/// threads, load 4.0 = 25) look four times busier than a 2-vCPU VPS
/// (load 1.0 = 50) while doing less work — placement compares nodes,
/// not kernels. Everything above the CPU count is saturation; one
/// number, not a curve, is all "least-loaded" needs.
///
/// `None` = unknown (an OS we cannot read): never mistaken for idle —
/// the registry column stays NULL and `any` placement skips the node's
/// score rather than treating unreadable as free.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoadOne {
    pub value: u8,
}

/// Read the 1-minute load average, normalized as above. Linux and the
/// BSDs: /proc/loadavg, then getloadavg(3) as the portable fallback;
/// elsewhere (or unreadable) None.
pub fn probe_load_one() -> Option<LoadOne> {
    probe_load_one_with(loadavg_one, cpu_count)
}

fn probe_load_one_with(
    read_one: impl Fn() -> Option<f64>,
    cpus: impl Fn() -> Option<usize>,
) -> Option<LoadOne> {
    let one = read_one()?;
    let cpus = cpus().filter(|&c| c > 0)? as f64;
    let normalized = (one / cpus * 100.0).clamp(0.0, 100.0);
    Some(LoadOne {
        value: normalized as u8,
    })
}

fn cpu_count() -> Option<usize> {
    std::thread::available_parallelism().ok().map(|n| n.get())
}

/// The raw 1-minute figure: /proc/loadavg where it exists (Linux, and
/// some BSDs mount it), else getloadavg(3). Both read KERNEL state, no
/// subprocess.
///
/// NOT on Android: the `libc` crate declares `getloadavg` for linux,
/// the BSDs and others but not for `target_os = "android"` (0.2.189),
/// so B4's plain `cfg(unix)` broke the phone build (PLAN-android A1).
/// Android reads /proc/loadavg only.
#[cfg(all(unix, not(target_os = "android")))]
fn loadavg_one() -> Option<f64> {
    if let Some(v) = read_proc_loadavg() {
        return Some(v);
    }
    let mut v = [0.0f64; 3];
    // getloadavg returns how many of the requested samples it wrote.
    let got = unsafe { libc::getloadavg(v.as_mut_ptr(), 3) };
    (got >= 1)
        .then_some(v[0])
        .filter(|x| x.is_finite() && *x >= 0.0)
}

#[cfg(any(not(unix), target_os = "android"))]
fn loadavg_one() -> Option<f64> {
    read_proc_loadavg()
}

fn read_proc_loadavg() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    parse_finite(text.split_whitespace().next()?)
}

fn parse_finite(s: &str) -> Option<f64> {
    let v = s.parse::<f64>().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// Probe for an NVIDIA GPU by asking `nvidia-smi`. Returns `None` when
/// the binary is absent, errors, reports no devices, or does not answer
/// within [`PROBE_TIMEOUT`] — every one of those collapses to "cannot
/// take GPU work".
///
/// This shells out rather than linking NVML: a subprocess WITH A
/// DEADLINE is a far smaller blast radius than an FFI dependency for one
/// CSV row. The deadline is the point — a driver wedged mid-suspend is a
/// real laptop state, and the first version said it had a timeout while
/// calling `.output()`, which waits forever (reproduced with a fake
/// `nvidia-smi` that sleeps: `veles nodes whoami` hung until killed).
pub fn probe_gpu() -> Option<GpuInfo> {
    probe_gpu_with("nvidia-smi", PROBE_TIMEOUT)
}

fn probe_gpu_with(bin: &str, timeout: std::time::Duration) -> Option<GpuInfo> {
    use std::io::Read;
    let mut child = std::process::Command::new(bin)
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                // Over the deadline (or unwaitable): kill AND reap, so a
                // wedged probe leaves neither a hang nor a zombie.
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    "node: {bin} did not answer within {timeout:?} — advertising no GPU"
                );
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    let first = text.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    let mut parts = first.splitn(2, ',');
    let name = parts.next()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let memory_total_mb = parts.next().and_then(|m| m.trim().parse::<u64>().ok());
    Some(GpuInfo {
        name,
        memory_total_mb,
    })
}

/// The programs a node says it has (PLAN-phone-userland.md T2): what
/// the automatic homes (`remotetool::NEEDS`) and `delegate_task needs`
/// route on. Probed once at start, by PATH lookup — no subprocess, so
/// the probe is bounded by one stat per name per PATH entry (the
/// nvidia-smi probe above needed a deadline). The one exception is a
/// phone whose shell is the userland's ulsh: one guest session, under a
/// deadline like nvidia-smi's, asks the Alpine rootfs (`userland::
/// probe_guest`), once per process.
pub const PROBED_TOOLS: &[&str] = &[
    "bash",
    "sh",
    "git",
    "rg",
    "jq",
    "curl",
    "ssh",
    "sqlite3",
    "tclsh",
    "podman",
    "systemd-nspawn",
    "firecracker",
    "jail",
    "agent-browser",
    "edge-tts",
    "xclip",
    "xsel",
    "wl-paste",
    "python3",
    "node",
    "proot",
    "apk",
    // Not a program: the phone's userland as this node's sandbox runtime
    // (PLAN-phone-userland.md T3 design item 4), advertised only while it
    // makes this node's boxes ([`probe_tools`]), never found on PATH.
    "userland",
];

/// [`PROBED_TOOLS`] entries that are not programs, so no PATH lookup
/// may claim them.
const NOT_PROGRAMS: &[&str] = &["userland"];

/// Programs the engine runs by a path from the environment rather than
/// PATH (the phone's `VELES_RG`/`VELES_TCLSH`, a podman outside PATH):
/// the variable naming an existing file counts as having the program.
const TOOL_ENV: &[(&str, &str)] = &[
    ("VELES_RG", "rg"),
    ("VELES_TCLSH", "tclsh"),
    ("VELES_PODMAN_BIN", "podman"),
];

/// The [`PROBED_TOOLS`] this machine has, in that list's order: what
/// PATH has here, plus, on a phone whose shell is the userland's ulsh,
/// what its Alpine rootfs has (the terminal's commands run there: `apk`,
/// and `bash` once installed), plus `userland` while the userland makes
/// this node's boxes (`[sandbox] backend = "userland"`).
pub fn probe_tools() -> Vec<String> {
    let host = probe_tools_with(
        |n| crate::shell::which(n).is_some(),
        |k| std::env::var(k).ok(),
    );
    let boxes = matches!(
        crate::user_envs::runtime(),
        crate::user_envs::Runtime::Userland(_)
    );
    merge_tools(&host, &guest_tools(), boxes)
}

/// The userland rootfs's [`PROBED_TOOLS`], probed through ulsh once per
/// process — one guest session, the only subprocess this probe starts,
/// and only where the shell is ulsh; empty elsewhere.
fn guest_tools() -> Vec<String> {
    static GUEST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    GUEST
        .get_or_init(|| match crate::userland::ulsh() {
            Some(ulsh) => {
                let names: Vec<&str> = PROBED_TOOLS
                    .iter()
                    .copied()
                    .filter(|n| !NOT_PROGRAMS.contains(n))
                    .collect();
                crate::userland::probe_guest(&ulsh, &names, std::time::Duration::from_secs(10))
            }
            None => Vec::new(),
        })
        .clone()
}

/// What this node advertises from its parts: the host's tools, the
/// userland guest's, and `userland` when it makes the boxes — in
/// [`PROBED_TOOLS`]' order, each once.
fn merge_tools(host: &[String], guest: &[String], userland_boxes: bool) -> Vec<String> {
    PROBED_TOOLS
        .iter()
        .filter(|n| {
            host.iter().any(|t| t == *n)
                || guest.iter().any(|t| t == *n)
                || (**n == "userland" && userland_boxes)
        })
        .map(|s| s.to_string())
        .collect()
}

/// [`probe_tools`]' PATH half, with PATH and the environment injected.
fn probe_tools_with(
    found: impl Fn(&str) -> bool,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    PROBED_TOOLS
        .iter()
        .filter(|name| !NOT_PROGRAMS.contains(name))
        .filter(|name| {
            found(name)
                || TOOL_ENV.iter().any(|(var, prog)| {
                    prog == *name
                        && env(var).is_some_and(|v| !v.trim().is_empty() && found(v.trim()))
                })
        })
        .map(|s| s.to_string())
        .collect()
}

/// The resolved capabilities this node advertises in the registry:
/// the probe merged under the `[node]` config overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCaps {
    pub gpu: Option<GpuInfo>,
    pub uptime_class: String,
    /// `linux`, `android`, `windows`, `freebsd` (`platform::OS`), or what
    /// `[node.caps] os` says.
    #[serde(default)]
    pub os: String,
    /// The [`PROBED_TOOLS`] this node has, or what `[node.caps] tools`
    /// says.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Free-form extras from `[node.caps]`, surfaced verbatim.
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

impl NodeCaps {
    /// Resolve capabilities from config + a live probe.
    ///
    /// Precedence: a `[node.caps] gpu = …` override WINS over the probe
    /// (you can lie up — claim a GPU the probe missed — or down —
    /// declare none to take the node out of GPU rotation despite the
    /// card). The probe only fills what the config did not say.
    pub fn resolve(cfg: &crate::config::NodeSection) -> Self {
        Self::resolve_with(cfg, probe_gpu, probe_tools)
    }

    /// What the config alone says (no GPU or PATH probe): the fallback
    /// for a caller whose probe could not run.
    pub fn unprobed(cfg: &crate::config::NodeSection) -> Self {
        Self::resolve_with(cfg, || None, Vec::new)
    }

    /// `resolve` with the probes injected, so the precedence can be
    /// pinned without a GPU (or a hanging driver) on the test machine.
    fn resolve_with(
        cfg: &crate::config::NodeSection,
        probe: impl FnOnce() -> Option<GpuInfo>,
        probe_tools: impl FnOnce() -> Vec<String>,
    ) -> Self {
        let mut extra = cfg.caps.clone();
        // `tools` and `os` win over the probe the way `gpu` does: lie up
        // (a program outside PATH) or down (keep work off a node). A
        // value that is not a list of names is said out loud and the
        // probe stands — failing closed here would advertise NOTHING,
        // and a node that claims no shell sends its own calls away.
        let tools = match extra.remove("tools") {
            None => probe_tools(),
            Some(serde_json::Value::Array(items))
                if items.iter().all(serde_json::Value::is_string) =>
            {
                let mut names: Vec<String> = items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                names.dedup();
                names
            }
            Some(v) => {
                tracing::warn!(
                    "[node.caps] tools = {v} is not a list of program names like \
                     [\"bash\", \"rg\"] — advertising what the probe found instead"
                );
                probe_tools()
            }
        };
        let os = match extra.remove("os") {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            None => crate::platform::OS.to_string(),
            Some(v) => {
                tracing::warn!(
                    "[node.caps] os = {v} is not a name like \"android\" — advertising {}",
                    crate::platform::OS
                );
                crate::platform::OS.to_string()
            }
        };
        // The opt-out has to be something a config.toml can SAY. The
        // first version documented `gpu = null`, which is not TOML at all,
        // and `gpu = false` failed to parse as a GpuInfo and fell back to
        // the probe — so the card stayed advertised (reproduced live:
        // `[node.caps] gpu = false` → `veles nodes whoami` showed the
        // RTX 3080). Now `false` and `"none"` opt out, a table declares a
        // GPU, and anything else is a config error said out loud and
        // treated as NO GPU — fail closed, never "probe instead".
        let gpu = match extra.remove("gpu") {
            None => probe(),
            Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false)) => None,
            Some(serde_json::Value::String(s)) if s.eq_ignore_ascii_case("none") => None,
            Some(v) => match serde_json::from_value::<GpuInfo>(v.clone()) {
                Ok(g) => Some(g),
                Err(e) => {
                    tracing::warn!(
                        "[node.caps] gpu = {v} is neither false, \"none\" nor a table like \
                         {{ name = \"…\", memory_total_mb = N }} ({e}) — advertising NO GPU"
                    );
                    None
                }
            },
        };
        let uptime_class = cfg
            .uptime_class
            .clone()
            .unwrap_or_else(|| "ephemeral".to_string());
        NodeCaps {
            gpu,
            uptime_class,
            os,
            tools,
            extra,
        }
    }

    /// `caps` as it is stored in the registry: the extras, plus `os`
    /// and `tools`, as one JSON object string. Folded into the column
    /// that already travels (node_register, gossip's `caps`), so neither
    /// needed a new field and an older peer passes the object through.
    pub fn caps_json(&self) -> Option<String> {
        let mut all: serde_json::Map<String, serde_json::Value> = self
            .extra
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        all.insert("os".into(), serde_json::Value::String(self.os.clone()));
        all.insert("tools".into(), serde_json::json!(self.tools));
        serde_json::to_string(&all).ok()
    }

    /// Whether this node has one of `names` (an any-of set, as
    /// `remotetool::NEEDS` writes them).
    pub fn has_any(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.tools.iter().any(|t| t == n))
    }

    pub fn gpu_json(&self) -> Option<String> {
        self.gpu
            .as_ref()
            .and_then(|g| serde_json::to_string(g).ok())
    }
}

/// Reading [`NodeCaps::caps_json`] back off a registry row. Here, not in
/// store.rs: the store carries the column as raw text and keeps no
/// opinion about the probe's schema; this module wrote it.
impl crate::store::NodeRow {
    /// The `caps` column as a JSON object; None when NULL or unreadable.
    fn caps_object(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        match serde_json::from_str(self.caps.as_deref()?).ok()? {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    /// The programs the node advertises (`NodeCaps::tools`,
    /// PLAN-phone-userland.md T2). `None` when the row says nothing — a
    /// node on an older build — which routing reads as "unknown", never
    /// as "has nothing".
    pub fn caps_tools(&self) -> Option<Vec<String>> {
        let tools = self.caps_object()?.remove("tools")?;
        Some(
            tools
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    }

    /// The node's OS (`NodeCaps::os`), when it advertises one.
    pub fn caps_os(&self) -> Option<String> {
        self.caps_object()?
            .get("os")?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Whether the node advertises one of `names` (any-of). False for a
    /// row that advertises nothing.
    pub fn caps_has_any(&self, names: &[&str]) -> bool {
        self.caps_tools()
            .is_some_and(|t| names.iter().any(|n| t.iter().any(|h| h == n)))
    }
}

/// Everything this node advertises, resolved WITHOUT touching the
/// store: identity (minted on first boot), capability probe, display
/// name. Split from [`register_resolved`] so the gateway runs the probe
/// — a subprocess that may take up to [`PROBE_TIMEOUT`] — without
/// holding the store lock. The first version probed INSIDE the lock, so
/// a slow driver stalled every store user in the gateway, not just boot.
pub struct SelfNode {
    pub identity: NodeIdentity,
    pub name: String,
    pub caps: NodeCaps,
    pub endpoint: Option<String>,
    /// This node's static WAN address and where that claim came from
    /// (B1.5) — see [`resolve_static_ip`].
    pub static_ip: Option<(String, StaticIpSource)>,
}

/// Where a registry row's `static_ip` came from, stored beside it so a
/// reader never has to guess what kind of claim it is holding.
///
/// Today there is one honest source: the operator's `[node] static_ip`,
/// a promise that the node's WAN address is stable (IRC CertFP/SASL
/// placement relies on exactly that). Nothing on the host can observe
/// it — the WAN address is what PEERS see a connection arrive from,
/// and whether it is static is a property of the uplink/trunk, not of
/// local configuration. The peer-observed source (a reflexive address
/// recorded from authenticated node-to-node calls, stable across
/// observations) is B3a's, when nodes talk regularly — PLAN-compute-pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticIpSource {
    Asserted,
}

impl StaticIpSource {
    pub fn as_str(self) -> &'static str {
        match self {
            StaticIpSource::Asserted => "asserted",
        }
    }
}

/// The static WAN address this node can promise the pool: the
/// operator's `[node] static_ip`, or nothing. The local route probe is
/// deliberately NOT a fallback — it sees the default route's SOURCE
/// address, which behind NAT is a LAN address (it registered
/// `192.168.1.50` for this laptop), and says nothing about the WAN IP
/// or its stability. One resolution for the registry row and
/// `veles nodes whoami`, so they can never disagree.
pub fn resolve_static_ip(node: &crate::config::NodeSection) -> Option<(String, StaticIpSource)> {
    match node.static_ip.as_deref().map(str::trim) {
        Some(a) if !a.is_empty() => Some((a.to_string(), StaticIpSource::Asserted)),
        _ => None,
    }
}

pub fn resolve_self(cfg: &crate::config::VelesConfig) -> Result<SelfNode> {
    let identity = NodeIdentity::load_or_create(&crate::config::config_dir())?;
    let caps = NodeCaps::resolve(&cfg.node);
    let name = match cfg.node.name.as_deref() {
        Some(n) if !n.trim().is_empty() => n.to_string(),
        _ => identity.name.clone(),
    };
    // Asserted only: placement (IRC CertFP/SASL) needs a promise about
    // the WAN address over time, and no local probe can make one.
    let static_ip = resolve_static_ip(&cfg.node);
    Ok(SelfNode {
        identity,
        name,
        caps,
        endpoint: cfg.node.endpoint.clone(),
        static_ip,
    })
}

/// The LOCAL route source: the address the default route would send
/// from. Behind NAT this is a LAN address, so it is neither the WAN IP
/// nor any evidence about whether the WAN IP is static — it is shown by
/// `veles nodes whoami` as what it is, and never enters the registry.
/// Probed, not dialed: `ip route get` answers from the kernel's routing
/// table without sending a packet.
pub fn probe_route_source() -> Option<String> {
    probe_route_source_with("ip", &["route", "get", "8.8.8.8"])
}

/// The injectable core, so the parse can be pinned without depending
/// on this test machine's routing table.
fn probe_route_source_with(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_route_src(&text)
}

/// Pull `src <addr>` out of `ip route get` output. The word must be
/// matched, not index-split: `ip route get` output is
/// space-separated `key value` pairs whose ORDER varies by kernel and
/// table state, so a fixed index is a coin flip. A leading
/// `8.8.8.8 via …` line does not contain `src`; the pair scan does.
pub(crate) fn parse_route_src(text: &str) -> Option<String> {
    let mut words = text.split_whitespace();
    while let Some(w) = words.next() {
        if w == "src" {
            let addr = words.next()?;
            if addr.is_empty() {
                return None;
            }
            return Some(addr.to_string());
        }
    }
    None
}

/// Write a resolved node into the registry — the short, locked half.
pub fn register_resolved(store: &crate::store::Store, me: &SelfNode) -> Result<()> {
    store.node_register(
        &me.identity.id,
        &me.name,
        &hostname(),
        env!("CARGO_PKG_VERSION"),
        &me.caps.uptime_class,
        me.caps.gpu_json().as_deref(),
        me.endpoint.as_deref(),
        me.static_ip
            .as_ref()
            .map(|(addr, source)| (addr.as_str(), source.as_str())),
        me.caps.caps_json().as_deref(),
    )
}

/// Resolve + register in one call, for callers that hold no shared lock
/// (`veles nodes probe`).
pub fn register_self(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
) -> Result<NodeIdentity> {
    let me = resolve_self(cfg)?;
    register_resolved(store, &me)?;
    Ok(me.identity)
}

// ── bootstrap seam (Plan B → Plan C) ────────────────────────────────

/// How a node learns the rest of the pool. B1 ships exactly one
/// strategy (static seeds); the trait is the seam a DHT backend (Plan
/// C) implements later, so "add a DHT" is a new impl + a config value,
/// not a rewrite of the joining path.
pub trait Bootstrap {
    /// Human name, for `veles nodes` and refusal messages.
    fn name(&self) -> &'static str;
    /// The addresses this node should try first. Empty is a legitimate
    /// answer on a single-node install (nothing to dial).
    fn seeds(&self) -> Vec<String>;
}

/// `cluster.bootstrap = "static"`: dial the configured seed
/// addresses. The always-on node's static IP is the natural seed —
/// this is where the VPS's one structural advantage is actually load
/// -bearing.
pub struct StaticBootstrap {
    pub seeds: Vec<String>,
}

impl Bootstrap for StaticBootstrap {
    fn name(&self) -> &'static str {
        "static"
    }
    fn seeds(&self) -> Vec<String> {
        self.seeds.clone()
    }
}

/// Resolve the configured bootstrap strategy, refusing an unknown name
/// the same way `validate_api_modes` refuses a wire we do not speak —
/// a bootstrap you typo'd should fail loudly at boot, not silently
/// isolate the node.
pub fn bootstrap_from(cfg: &crate::config::ClusterSection) -> Result<Box<dyn Bootstrap>> {
    match cfg.bootstrap.trim() {
        "" | "static" => Ok(Box::new(StaticBootstrap {
            seeds: cfg.seeds.clone(),
        })),
        other => anyhow::bail!(
            "[cluster] bootstrap = \"{other}\" is not implemented — this build speaks \
             only \"static\". A DHT backend (Plan C) is the intended later value; \
             remove the line or set it to \"static\" until then."
        ),
    }
}

/// Does this node declare membership in a pool? Any peer list set:
/// seeds, the sync/gossip peers, or the peers allowed to run work here.
/// The singleton fence's rule: a pool member with no arbiter REFUSES
/// cron/telegram/IRC (two such nodes are the double-poller); a node
/// with no peers at all runs them ungated.
pub fn pool_declared(c: &crate::config::ClusterSection) -> bool {
    !c.seeds.is_empty()
        || !c.memsync_peers.is_empty()
        || !c.gossip_peers.is_empty()
        || !c.task_peers.is_empty()
}

/// The `/nodes` arbiter line when `[cluster] arbiter` is unset. B3a
/// printed "no node may run cron/telegram/IRC" everywhere — including
/// on a single node, which runs them ungated, so the one status line
/// meant to be trusted was wrong on every standalone install.
pub fn arbiter_unset_status(c: &crate::config::ClusterSection) -> String {
    if pool_declared(c) {
        "arbiter: NOT SET — this node has pool peers, so cron/telegram/IRC are REFUSED \
         here. Set [cluster] arbiter = <node-id> on the always-on node."
            .into()
    } else {
        "arbiter: not set — single node: cron/telegram/IRC run here ungated. Set \
         [cluster] arbiter before adding a second node."
            .into()
    }
}

/// Validate `[cluster] arbiter` (B1.5): ONE configured node id or
/// unique prefix, and it must RESOLVE against the registry. An arbiter
/// nobody can resolve is the fail-closed lease with the safe node
/// configured wrong — the catastrophic double-poller setup, caught at
/// config-check time instead of at the 3am partition. Returns the
/// resolved full id on success.
///
/// Deliberately a separate call from `load()`: the registry is runtime
/// state, and a config check that refused to boot because a peer's row
/// was missing would make `arbiter` un-settable on a fresh node. The
/// CLI's `nodes` family and B3a's lease grant both call this.
pub fn validate_arbiter(
    cfg: &crate::config::ClusterSection,
    store: &crate::store::Store,
) -> Result<String> {
    let raw = match cfg.arbiter.as_deref().map(str::trim) {
        None => anyhow::bail!("[cluster] arbiter is not set — no node may run the singletons (cron, telegram, IRC) until one is named. This is the fail-closed default; set it on the always-on node's id."),
        Some("") => anyhow::bail!("[cluster] arbiter is empty — same as unset. Name the always-on node's id (see `veles nodes whoami` on it)."),
        Some(a) => a,
    };
    match store.node_resolve(raw)? {
        crate::store::NodeMatch::One(row) => Ok(row.id),
        crate::store::NodeMatch::None => anyhow::bail!(
            "[cluster] arbiter = \"{raw}\" matches no node in this registry — run `veles nodes probe` on the intended node first, or fix the id"
        ),
        crate::store::NodeMatch::Ambiguous(n) => anyhow::bail!(
            "[cluster] arbiter = \"{raw}\" matches {n} nodes — give more of the id"
        ),
    }
}

/// Whether a registry heartbeat is older than the staleness budget.
/// `heartbeat_at` is SQLite's `datetime('now')` ("YYYY-MM-DD HH:MM:SS",
/// UTC). A heartbeat we cannot parse is treated as stale, never fresh —
/// "is this node alive" must fail closed. Shared by the CLI (`veles
/// nodes`) and the bridge (`veles::nodes`) so the two surfaces agree.
pub fn is_stale(heartbeat_at: &str, stale_after_secs: i64) -> bool {
    let Ok(ts) = chrono::NaiveDateTime::parse_from_str(heartbeat_at, "%Y-%m-%d %H:%M:%S") else {
        return true;
    };
    let age = chrono::Utc::now().naive_utc() - ts;
    age.num_seconds() > stale_after_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity persists: load-or-create twice in the same dir must
    /// return the SAME id, not mint a fresh one. A node whose identity
    /// changes every boot fragments the registry into one ghost row per
    /// restart — the pool could never tell "laptop came back" from "a
    /// new laptop appeared".
    #[test]
    fn identity_persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let a = NodeIdentity::load_or_create(dir.path()).unwrap();
        let b = NodeIdentity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.id, b.id, "identity must survive a re-read");
        assert!(!a.id.is_empty());
        // And it must be a parseable UUID — the registry key is not
        // free-text a later writer could collide on.
        assert!(uuid::Uuid::parse_str(&a.id).is_ok());
    }

    /// Two different config dirs mint two different identities — the
    /// whole point of keeping the id OUT of config.toml: copying a
    /// config between machines must give each its own node id, not
    /// carry one across.
    #[test]
    fn identity_is_per_machine_not_per_config() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = NodeIdentity::load_or_create(d1.path()).unwrap();
        let b = NodeIdentity::load_or_create(d2.path()).unwrap();
        assert_ne!(a.id, b.id, "separate config homes get separate identities");
    }

    fn node_section(toml_text: &str) -> crate::config::NodeSection {
        let cfg: crate::config::VelesConfig = toml::from_str(toml_text).unwrap();
        cfg.node
    }

    fn a_probe_that_finds_a_card() -> Option<GpuInfo> {
        Some(GpuInfo {
            name: "Probed RTX".into(),
            memory_total_mb: Some(16384),
        })
    }

    /// A `[node.caps] gpu` override wins over the probe, in BOTH
    /// directions — written as the TOML a user can actually type, and
    /// against a probe that DOES find a card, so "fell back to the probe"
    /// cannot pass as "opted out". The first version of this pin built
    /// `Value::Null` in Rust: TOML has no null, and the `gpu = false` a
    /// user would write kept the card advertised.
    #[test]
    fn caps_override_beats_probe_both_ways() {
        let declared = node_section(
            "[node.caps]\ngpu = { name = \"Remote A100\", memory_total_mb = 40960 }\n",
        );
        let caps = NodeCaps::resolve_with(&declared, a_probe_that_finds_a_card, no_tools);
        assert_eq!(caps.gpu.as_ref().unwrap().name, "Remote A100");
        assert_eq!(caps.gpu.unwrap().memory_total_mb, Some(40960));

        for opt_out in ["gpu = false", "gpu = \"none\"", "gpu = \"NONE\""] {
            let s = node_section(&format!("[node.caps]\n{opt_out}\n"));
            let caps = NodeCaps::resolve_with(&s, a_probe_that_finds_a_card, no_tools);
            assert!(caps.gpu.is_none(), "`{opt_out}` must opt out");
        }

        // Unset: the probe decides.
        let caps = NodeCaps::resolve_with(&node_section(""), a_probe_that_finds_a_card, no_tools);
        assert_eq!(caps.gpu.unwrap().name, "Probed RTX");
    }

    fn no_tools() -> Vec<String> {
        Vec::new()
    }

    fn a_probe_that_finds_bash_and_git() -> Vec<String> {
        vec!["bash".into(), "git".into()]
    }

    /// `[node.caps] tools` / `os` win over the probe like `gpu` does
    /// (PLAN-phone-userland.md T2), written as the TOML a user types and
    /// against a probe that DOES find programs, so "fell back to the
    /// probe" cannot pass as "overridden". Unset, the probe decides and
    /// the OS is this build's.
    #[test]
    fn tools_and_os_override_beats_probe_both_ways() {
        let s = node_section("[node.caps]\ntools = [\"rg\", \"podman\"]\nos = \"android\"\n");
        let caps = NodeCaps::resolve_with(&s, || None, a_probe_that_finds_bash_and_git);
        assert_eq!(caps.tools, vec!["rg".to_string(), "podman".to_string()]);
        assert_eq!(caps.os, "android");
        assert!(
            !caps.extra.contains_key("tools") && !caps.extra.contains_key("os"),
            "the overrides are fields, not extras"
        );

        // Down: an empty list advertises nothing, probe or not.
        let s = node_section("[node.caps]\ntools = []\n");
        let caps = NodeCaps::resolve_with(&s, || None, a_probe_that_finds_bash_and_git);
        assert!(caps.tools.is_empty());

        // Unset: the probe decides, the OS is this build's.
        let caps =
            NodeCaps::resolve_with(&node_section(""), || None, a_probe_that_finds_bash_and_git);
        assert_eq!(caps.tools, vec!["bash".to_string(), "git".to_string()]);
        assert_eq!(caps.os, crate::platform::OS);

        // Malformed: the probe stands (a node claiming no shell would
        // send its own calls away).
        let s = node_section("[node.caps]\ntools = \"rg\"\n");
        let caps = NodeCaps::resolve_with(&s, || None, a_probe_that_finds_bash_and_git);
        assert_eq!(caps.tools, vec!["bash".to_string(), "git".to_string()]);
    }

    /// The probe finds programs on PATH and, for the ones the engine
    /// runs by a path from the environment (the phone's VELES_RG and
    /// VELES_TCLSH), the variable naming a file counts as present.
    #[test]
    fn probe_counts_path_and_the_env_overrides() {
        let on_path = |n: &str| n == "bash" || n == "/apk/lib/librg.so";
        let env = |k: &str| match k {
            "VELES_RG" => Some("/apk/lib/librg.so".to_string()),
            // Named but not a file: not present.
            "VELES_TCLSH" => Some("/nowhere/tclsh".to_string()),
            _ => None,
        };
        let got = probe_tools_with(on_path, env);
        assert_eq!(got, vec!["bash".to_string(), "rg".to_string()]);
    }

    /// The phone with its userland (PLAN-phone-userland.md T3 design item
    /// 5): what the guest has counts (the terminal's commands run there),
    /// and `userland` is advertised only while it makes this node's boxes
    /// — never because PATH has something of that name.
    #[test]
    fn the_userland_adds_its_guests_tools_and_itself_as_a_runtime() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let host = s(&["bash", "git", "rg", "jq", "curl", "ssh", "sqlite3", "tclsh"]);
        let guest = s(&["sh", "bash", "git", "apk", "jq"]);
        assert_eq!(
            merge_tools(&host, &guest, true),
            s(&[
                "bash", "sh", "git", "rg", "jq", "curl", "ssh", "sqlite3", "tclsh", "apk",
                "userland"
            ]),
            "PROBED_TOOLS' order, each once"
        );
        let shell_only = merge_tools(&host, &guest, false);
        assert!(shell_only.contains(&"apk".to_string()));
        assert!(!shell_only.contains(&"userland".to_string()));
        assert_eq!(
            merge_tools(&host, &[], false),
            host,
            "no userland: the host's"
        );
        let named = probe_tools_with(|n| n == "userland" || n == "sh", |_| None);
        assert_eq!(
            named,
            s(&["sh"]),
            "a program called userland is not the runtime"
        );
    }

    /// `os` and `tools` ride in the caps column (no schema change), and
    /// a registry row reads them back — through gossip too, since the
    /// wire row carries the column as it is.
    #[test]
    fn caps_json_carries_os_and_tools_through_the_registry_and_gossip() {
        let s =
            node_section("[node.caps]\ntools = [\"bash\", \"rg\"]\nos = \"android\"\nmic = true\n");
        let caps = NodeCaps::resolve_with(&s, || None, no_tools);
        let dir = tempfile::tempdir().unwrap();
        let a = crate::store::Store::open(&dir.path().join("a.db")).unwrap();
        a.node_register(
            "phone-1",
            "phone",
            "h",
            "9",
            "ephemeral",
            None,
            None,
            None,
            caps.caps_json().as_deref(),
        )
        .unwrap();
        let row = a.node_get("phone-1").unwrap().unwrap();
        assert_eq!(
            row.caps_tools(),
            Some(vec!["bash".to_string(), "rg".to_string()])
        );
        assert_eq!(row.caps_os().as_deref(), Some("android"));
        assert!(row.caps_has_any(&["rg", "ripgrep"]));
        assert!(!row.caps_has_any(&["podman"]));

        // Gossip: the wire row, merged into another node's registry.
        let wire = crate::store::GossipNodeRow::from_row(&row);
        let json = serde_json::to_string(&wire).unwrap();
        let back: crate::store::GossipNodeRow = serde_json::from_str(&json).unwrap();
        let b = crate::store::Store::open(&dir.path().join("b.db")).unwrap();
        b.node_merge_from_gossip(&back).unwrap();
        let there = b.node_get("phone-1").unwrap().unwrap();
        assert_eq!(there.caps_tools(), row.caps_tools());
        assert_eq!(there.caps_os().as_deref(), Some("android"));

        // An older node's row says nothing: unknown, not empty.
        let old = crate::store::NodeRow { caps: None, ..row };
        assert_eq!(old.caps_tools(), None);
        assert!(!old.caps_has_any(&["bash"]));
    }

    /// A value that is neither an opt-out nor a GPU table is a config
    /// error: advertised as NO GPU (fail closed), never "probe instead".
    #[test]
    fn a_malformed_gpu_override_fails_closed() {
        let s = node_section("[node.caps]\ngpu = \"RTX 4090\"\n");
        let caps = NodeCaps::resolve_with(&s, a_probe_that_finds_a_card, no_tools);
        assert!(caps.gpu.is_none());
    }

    /// B4: the load metric is normalized BY CPU COUNT before it enters
    /// the registry — the whole reason `any` placement can compare a
    /// 16-thread laptop with a 2-vCPU VPS. Raw loadavg here would make
    /// the laptop look twice as idle at four times the work. Clamped
    /// at 100 (saturation reads as saturation, not as 1000), and an
    /// unreadable load is None — unknown, never 0: zero means idle,
    /// and placement must not read unknown as free.
    #[test]
    fn load_is_normalized_by_cpu_count_and_never_invents_idle() {
        // 4.0 load on 16 cpus = 25% — the laptop case.
        let l = probe_load_one_with(|| Some(4.0), || Some(16)).unwrap();
        assert_eq!(l.value, 25);
        // The SAME raw load on 2 cpus = 100%: raw numbers lie,
        // normalized ones compare.
        let l = probe_load_one_with(|| Some(4.0), || Some(2)).unwrap();
        assert_eq!(l.value, 100);
        // Saturation clamps; it does not wrap or overflow.
        let l = probe_load_one_with(|| Some(200.0), || Some(2)).unwrap();
        assert_eq!(l.value, 100);
        // Unreadable load / unreadable cpu count / zero cpus: None.
        assert!(probe_load_one_with(|| None, || Some(16)).is_none());
        assert!(probe_load_one_with(|| Some(1.0), || None).is_none());
        assert!(probe_load_one_with(|| Some(1.0), || Some(0)).is_none());
    }

    #[cfg(unix)]
    fn fake_smi(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("nvidia-smi");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// A probe that does not answer is abandoned at its deadline. The
    /// first version called `.output()` — no deadline at all — and ran it
    /// while holding the gateway's store lock.
    #[cfg(unix)]
    #[test]
    fn a_hung_probe_is_abandoned_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let smi = fake_smi(dir.path(), "sleep 30");
        let t = std::time::Instant::now();
        let got = probe_gpu_with(smi.to_str().unwrap(), std::time::Duration::from_millis(300));
        assert!(got.is_none());
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "must return near the deadline, took {:?}",
            t.elapsed()
        );
    }

    /// …and a probe that answers is still parsed.
    #[cfg(unix)]
    #[test]
    fn a_prompt_probe_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let smi = fake_smi(dir.path(), "echo 'NVIDIA Test Card, 8192'");
        let got = probe_gpu_with(smi.to_str().unwrap(), PROBE_TIMEOUT).unwrap();
        assert_eq!(got.name, "NVIDIA Test Card");
        assert_eq!(got.memory_total_mb, Some(8192));
    }

    /// The read path does not mint an identity.
    #[test]
    fn load_does_not_mint() {
        let dir = tempfile::tempdir().unwrap();
        assert!(NodeIdentity::load(dir.path()).unwrap().is_none());
        assert!(!dir.path().join(NODE_FILE).exists());
        let minted = NodeIdentity::load_or_create(dir.path()).unwrap();
        assert_eq!(
            NodeIdentity::load(dir.path()).unwrap().unwrap().id,
            minted.id
        );
    }

    /// `uptime_class` defaults to ephemeral: a node that does not say
    /// it is always-on is treated as one that comes and goes. That is
    /// the safe default for placement — cron and the telegram poller
    /// must never land on a node that merely failed to declare itself.
    #[test]
    fn uptime_class_defaults_to_ephemeral() {
        let cfg = crate::config::NodeSection::default();
        assert_eq!(NodeCaps::resolve(&cfg).uptime_class, "ephemeral");
        let cfg = crate::config::NodeSection {
            uptime_class: Some("always-on".to_string()),
            ..Default::default()
        };
        assert_eq!(NodeCaps::resolve(&cfg).uptime_class, "always-on");
    }

    /// An unknown bootstrap strategy is refused by name at boot, not
    /// silently downgraded to static (which would isolate the node
    /// while looking configured). The refusal names the Plan C value so
    /// the config line tells you what you meant.
    #[test]
    fn unknown_bootstrap_is_refused_by_name() {
        let cfg = crate::config::ClusterSection {
            bootstrap: "dht".to_string(),
            ..Default::default()
        };
        let err = match bootstrap_from(&cfg) {
            Ok(_) => panic!("an unknown bootstrap must be refused, not resolved"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("dht"),
            "refusal names what you asked for: {err}"
        );
        assert!(err.contains("static"), "refusal names what works: {err}");
    }

    /// B1.5: the registry's `static_ip` is a CLAIM with a source, and
    /// the only source today is the operator's assertion. Unset (or
    /// blank) is NOTHING — never the local route probe, which registered
    /// a DHCP LAN address (`192.168.1.50`) as this laptop's static IP.
    #[test]
    fn static_ip_is_asserted_or_absent() {
        let asserted = crate::config::NodeSection {
            static_ip: Some(" 203.0.113.7 ".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_static_ip(&asserted),
            Some(("203.0.113.7".to_string(), StaticIpSource::Asserted))
        );
        let blank = crate::config::NodeSection {
            static_ip: Some("  ".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_static_ip(&blank), None);
        assert_eq!(
            resolve_static_ip(&crate::config::NodeSection::default()),
            None
        );
    }

    /// B1.5: `ip route get` output is key/value pairs in NO fixed
    /// order — `src` must be found by word, not by index. Both shapes
    /// are real: with a via-gateway and without.
    #[test]
    fn route_src_is_found_by_word_not_position() {
        let with_via = "8.8.8.8 via 192.168.1.1 dev wlp3s0 src 192.168.1.42 metric 600\n";
        assert_eq!(parse_route_src(with_via).as_deref(), Some("192.168.1.42"));
        let direct = "8.8.8.8 dev wg0 src 10.20.30.2 metric 100\n";
        assert_eq!(parse_route_src(direct).as_deref(), Some("10.20.30.2"));
        // No src pair at all (e.g. a broadcast route): honest None,
        // never a neighbouring word picked by accident.
        assert_eq!(parse_route_src("8.8.8.8 via 10.0.0.1 dev eth0\n"), None);
        assert_eq!(parse_route_src(""), None);
    }

    /// B1.5: the arbiter must resolve to EXACTLY one registry row.
    /// Unset/empty are refusals (fail-closed default), an unknown id
    /// is a refusal, and a unique prefix resolves to the full id.
    #[test]
    fn arbiter_must_resolve_to_exactly_one_node() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        store
            .node_register(
                "abcd-1111",
                "vps",
                "h",
                "0",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let unset = crate::config::ClusterSection::default();
        let err = validate_arbiter(&unset, &store).unwrap_err().to_string();
        assert!(err.contains("not set"), "unset is a refusal: {err}");

        let empty = crate::config::ClusterSection {
            arbiter: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(validate_arbiter(&empty, &store).is_err(), "empty = unset");

        let missing = crate::config::ClusterSection {
            arbiter: Some("zzzz".to_string()),
            ..Default::default()
        };
        let err = validate_arbiter(&missing, &store).unwrap_err().to_string();
        assert!(err.contains("zzzz"), "refusal names the id: {err}");

        let prefix = crate::config::ClusterSection {
            arbiter: Some("abcd".to_string()),
            ..Default::default()
        };
        assert_eq!(
            validate_arbiter(&prefix, &store).unwrap(),
            "abcd-1111",
            "a unique prefix resolves to the full id"
        );
    }

    /// The other half of the bootstrap refusal test: static resolves
    /// and carries the seeds.
    #[test]
    fn static_bootstrap_resolves_and_carries_seeds() {
        let cfg = crate::config::ClusterSection {
            seeds: vec!["http://vps:9900".to_string()],
            ..Default::default()
        };
        let b = bootstrap_from(&cfg).unwrap();
        assert_eq!(b.name(), "static");
        assert_eq!(b.seeds(), vec!["http://vps:9900".to_string()]);
    }
    /// The unset-arbiter line tells the truth about THIS node: refused
    /// when it has pool peers, running ungated when it is alone.
    #[test]
    fn the_unset_arbiter_line_matches_what_the_fence_does() {
        let mut c = crate::config::ClusterSection::default();
        assert!(!pool_declared(&c));
        assert!(arbiter_unset_status(&c).contains("run here ungated"));
        c.task_peers = vec!["phone".into()];
        assert!(pool_declared(&c), "a task peer is a pool peer");
        assert!(arbiter_unset_status(&c).contains("REFUSED"));
    }
}
