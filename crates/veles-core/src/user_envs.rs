//! Persistent per-principal sandbox environments ("user envs").
//!
//! The ACL used to answer "may this principal touch the host?" with a
//! denial row — correct, but lame: a verified user could not run a
//! build or edit a file AT ALL below admin. This module is the other
//! answer: every principal below admin gets their own CONTAINER, and
//! the tool layer routes their terminal/shell/read/write/execute_code
//! calls into it transparently. The denial is replaced by a fence.
//!
//! # The zone contract
//!
//! [`zone_for`] is the ONE routing decision, consulted by every
//! env-touching tool at call time from the SESSION principal (never
//! from model-controlled args for sub-admins):
//!
//! - principal below `Role::Admin` ⇒ [`Zone::Sandbox`] keyed on the
//!   principal id — ALWAYS, an explicit `sandbox:false` is ignored;
//! - Admin/Owner/owner-surface ⇒ [`Zone::Host`], unless the call
//!   carries `sandbox:true`, which opts THAT call into the caller's
//!   own env (the model may ask; the framework decides who may not).
//!
//! Enforcement is therefore structural: the sandbox zone physically
//! has no secrets (podman exec passes NO gateway env), and the host
//! zone never executes for a below-admin principal. The capability
//! grants (`Shell`/`FileWrite` for `User`+) are safe by construction
//! — the cap buys a container, not the host.
//!
//! # The environment
//!
//! One rootless-podman container per principal:
//!
//! - name `veles-user-env-<key>` where `<key>` = sanitized principal
//!   id + a hash of (id, veles home). The home in the hash keeps test
//!   scratch homes from colliding with (or reusing) production envs.
//! - `<data_dir>/user-envs/<key>/work` bind-mounted at `/work`, with
//!   `HOME=/work` — the persistent workspace. It survives restarts,
//!   idle-stops and purges-that-keep-work; nothing in it is ever
//!   auto-deleted.
//! - created from `[sandbox] image` with the section's memory/network/
//!   capability posture. `[[sandbox.mounts]]` and `extra_args` are
//!   deliberately NOT inherited — those are operator grants for the
//!   per-run tcl fence; silently giving every unprivileged principal
//!   the same host binds would be a privilege change by config drift.
//! - lifecycle: created on first use, started when stopped, stopped
//!   after `user_env_idle_stop_secs` idle (0 = never), reaped if its
//!   workspace directory has vanished (a wiped scratch home must not
//!   leave zombie containers).
//!
//! On the phone, `[sandbox] backend = "userland"` makes the boxes the
//! userland package's instead ([`Runtime::Userland`],
//! PLAN-phone-userland.md T3 design item 4): each key its own proot guest
//! root, `files/sandboxes/<key>` in that app, made from a copy of its
//! rootfs on first use, with `/work` inside it and HOME there as here.
//! Every touch is a `ulsh --exec` with no environment of the engine's —
//! the podman-remote shape, since this app cannot see the other's files.
//! The files are apart; the fence is weaker (the phone's network and the
//! userland's uid for every box), so such a box never counts as fenced.
//!
//! # Why file ops go through `podman cp`, not the bind mount
//!
//! `/work` IS a host directory, so a host-side read of
//! `<root>/<key>/work/x` would see the same bytes — EXCEPT when `x`
//! is a symlink planted inside the container pointing at `/etc`. The
//! symlink is a string; the host kernel resolves it against the HOST
//! `/etc`, and a sub-admin read would walk straight out of the fence.
//! Every model-directed path therefore enters the container via
//! `podman cp` / `podman exec`, which resolve inside the container
//! namespace. The ONLY host-side touches are framework-generated
//! staging paths (`.veles-run/`, `.veles-bg/`, `.staging/`), each
//! canonicalize-guarded to stay under the workspace before use — the
//! same trick defeats a user pre-planting `.veles-run -> /etc` and
//! luring the framework itself into writing outside.

use crate::lockutil::LockOk;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Container/prefix contract. Everything the manager creates starts
/// with this, so `podman ps --filter name=` and manual cleanup have
/// one handle.
pub const NAME_PREFIX: &str = "veles-user-env-";
const VOLUME_PREFIX: &str = "veles-env-";

// Runtime ownership metadata, not configuration. A podman socket can
// be shared by gateways that see the same path (VPS production and its
// scratch gateway both see /opt/data). Missing labels never authorize
// automatic deletion, and node identity is part of the ownership key.
const NODE_LABEL: &str = "online.7778777.veles.user-env-node";
const HOME_LABEL: &str = "online.7778777.veles.user-env-home";
const INCARNATION_LABEL: &str = "online.7778777.veles.user-env-incarnation";

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvOwner {
    node: String,
    home: PathBuf,
    generation: String,
}

impl EnvOwner {
    fn labels(&self) -> [String; 3] {
        [
            format!("{NODE_LABEL}={}", self.node),
            format!("{HOME_LABEL}={}", self.home.display()),
            format!("{INCARNATION_LABEL}={}", self.generation),
        ]
    }
}

fn current_owner() -> Option<EnvOwner> {
    // The home may be wiped WHILE this process stays up. Remember the
    // node id it already used, but prefer the file when it exists so an
    // explicit restore/re-identification still wins. Two gateway
    // processes have independent maps even when both see /opt/data.
    static NODES: std::sync::OnceLock<Mutex<HashMap<PathBuf, String>>> = std::sync::OnceLock::new();
    let config = crate::config::config_dir();
    let on_disk = crate::node::NodeIdentity::load(&config)
        .ok()
        .flatten()
        .map(|identity| identity.id)
        .filter(|id| !id.trim().is_empty());
    let nodes = NODES.get_or_init(Default::default);
    let mut nodes = nodes.lock_ok();
    let node = match on_disk {
        Some(id) => {
            nodes.insert(config, id.clone());
            id
        }
        None => match nodes.get(&config).cloned() {
            Some(id) => id,
            None => {
                // First sandbox use may precede cluster bootstrap. Mint
                // the same persisted identity the pool uses rather than
                // creating an unlabelled box on a gateway that simply
                // has not run `nodes` yet.
                let id = crate::node::NodeIdentity::load_or_create(&config).ok()?.id;
                if id.trim().is_empty() {
                    return None;
                }
                nodes.insert(config, id.clone());
                id
            }
        },
    };
    drop(nodes);
    Some(EnvOwner {
        node,
        home: crate::config::veles_home(),
        generation: incarnation(),
    })
}

fn is_previous_incarnation(owner: &EnvOwner, current: &EnvOwner) -> bool {
    owner.node == current.node
        && owner.home == current.home
        && owner.generation != current.generation
}

/// May an automatic lifecycle operation replace this object? Legacy
/// resources with no labels remain reusable while they still fit; if
/// they must be rebuilt, an operator gets a reason instead of an
/// implicit adoption or a new container covering a foreign one.
fn rebuild_allowed(resource: &OwnedResource, current: &EnvOwner) -> Result<(), String> {
    match &resource.owner {
        Some(owner) if owner != current => {
            Err(format!("belongs to another node, home, or generation: {owner:?}"))
        }
        Some(_) => Ok(()),
        None if resource.ownership_incomplete => Err(
            "has incomplete ownership labels and cannot be automatically rebuilt; inspect it rather than treating it as legacy"
                .into(),
        ),
        None => Err(
            "has no ownership labels and cannot be automatically rebuilt; check it, then use the explicit purge path"
                .into(),
        ),
    }
}

/// Where the workspace lives inside every env container.
pub const CONTAINER_WORK: &str = "/work";

// ── zone: the routing decision ────────────────────────────────────────

/// Where an env-touching tool call executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zone {
    /// The host, exactly as pre-user-envs veles behaved.
    Host,
    /// A named persistent container. `name` is what people call it
    /// (`chan:irc/libera/#veles`, `shared`, `quarantine`); `key` is
    /// what the container and its workspace are named after, and is a
    /// pure function of the name plus this home and its incarnation.
    Sandbox { name: String, key: String },
}

impl Zone {
    /// The container key, for the calls that only need that.
    pub fn key(&self) -> Option<&str> {
        match self {
            Zone::Host => None,
            Zone::Sandbox { key, .. } => Some(key),
        }
    }
}

/// Where a call asked to run.
///
/// It began life as a bool named `sandbox`, which could only express
/// "in my own box" — an admin could opt IN but never OUT (they were on
/// the host by default), and nobody could name a box. Then the default
/// flipped and the polarity stopped making sense. USER, 2026-09-09:
/// *"I don't like that you have to specify `sandbox: true`, we should
/// fail closed and default to sandbox. `host: true` should be the flag
/// to execute on host."*
///
/// So the boolean the model sees is `host`, the absence of any answer
/// means the fence, and leaving it takes an explicit word. `sandbox`
/// survives as the place a NAME goes (`sandbox: "quarantine"`) and, for
/// rows written before this change — cron jobs persist the key — as an
/// undocumented bool alias.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ZoneArg {
    /// The call said nothing: the framework decides.
    #[default]
    Unset,
    /// `host: true` — this call runs on the host, if the caller may.
    Host,
    /// `host: false` — this call runs in the caller's own box, said out
    /// loud. Same answer the framework gives by default for a
    /// connector principal; worth having so a caller can be explicit.
    Own,
    /// `sandbox: "<name>"` — this call runs in that box, if the caller
    /// may reach it.
    Named(String),
}

impl ZoneArg {
    /// Read the zone off a tool call.
    ///
    /// A NAME wins over `host: true`. A call that says both is
    /// contradictory, and resolving the contradiction toward the fence
    /// is the fail-closed reading — the same instinct that makes junk
    /// mean `Unset` rather than "host".
    pub fn from_args(args: &Value) -> Self {
        if let Some(name) = args.get("sandbox").and_then(sandbox_name) {
            return ZoneArg::Named(name);
        }
        match args.get("host") {
            None | Some(Value::Null) => legacy_sandbox_bool(args),
            Some(Value::Bool(true)) => ZoneArg::Host,
            Some(Value::Bool(false)) => ZoneArg::Own,
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "host" => ZoneArg::Host,
                "false" | "no" | "off" | "sandbox" => ZoneArg::Own,
                _ => ZoneArg::Unset,
            },
            _ => ZoneArg::Unset,
        }
    }
}

/// A `sandbox` value that is a NAME rather than a bool in disguise.
fn sandbox_name(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    if s.is_empty() || BOOLISH.contains(&s.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(s.to_string())
}

const BOOLISH: &[&str] = &["true", "yes", "on", "false", "no", "off", "host", "sandbox"];

/// Drop a `host` request the caller may not have, in place.
///
/// Routing does not need this — `zone_for` returns a sandbox below
/// admin whatever the arguments said. The TRANSCRIPT does: the
/// arguments are recorded and re-read (by the model, by `/context`, by
/// a checkpoint), and a call that stands there saying `"host": true`
/// next to a result tagged `[env: sandbox …]` teaches every later turn
/// that the flag works. Removing it is the honest record of the
/// decision the framework already made.
///
/// A `sandbox: "<name>"` is left alone: whether they may reach that box
/// is the tool layer's question, and the answer is a visible denial.
/// Returns whether anything was removed.
pub fn strip_host_request(args: &mut Value) -> bool {
    args.as_object_mut()
        .and_then(|o| o.remove("host"))
        .is_some()
}

/// `sandbox: true|false`, the pre-2026-09-09 spelling.
///
/// Not in any schema any more, so the model is not TOLD about it. It is
/// still read because instructions outlive schemas: a skill, a custom
/// command, a plugin or a memory written against the old flag will keep
/// emitting `sandbox: true`, and dropping the key on the floor would
/// read that as "the framework decides" — which on a local surface
/// means the HOST. A stale instruction asking for the fence must not
/// silently get less of one. The polarity is preserved, not inverted:
/// `false` still means the host, for a caller allowed to have it.
fn legacy_sandbox_bool(args: &Value) -> ZoneArg {
    match args.get("sandbox") {
        Some(Value::Bool(true)) => ZoneArg::Own,
        Some(Value::Bool(false)) => ZoneArg::Host,
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "sandbox" => ZoneArg::Own,
            "false" | "no" | "off" | "host" => ZoneArg::Host,
            _ => ZoneArg::Unset,
        },
        _ => ZoneArg::Unset,
    }
}

/// The ONE routing decision.
///
/// ```text
/// role <  Admin                            -> Sandbox, ALWAYS
/// role >= Admin, [sandbox] admin_default_zone
///     = "sandbox" (the default)            -> Sandbox unless host:true
///     = "host"                             -> Host unless host:false|<name>
/// no principal, [sandbox] local_default_zone
///     = "host" (the default)               -> Host unless host:false|<name>
///     = "sandbox"                          -> Sandbox unless host:true
/// ```
///
/// A below-admin principal is sandboxed regardless of what the model
/// asks for, and a name they may not reach is ignored here — the tool
/// layer turns that into a visible denial (`deny_named_sandbox`) rather
/// than quietly running it somewhere else.
///
/// The BOX is the session's (`sandboxes::session_sandbox`, published by
/// the gateway when it resolves the conversation), not the principal's:
/// everyone talking to the bot in one channel shares one workspace,
/// which is what makes collaborative work possible at all.
pub fn zone_for(session_id: Option<&str>, arg: ZoneArg) -> Zone {
    let sid = session_id.filter(|s| !s.is_empty());
    let principal = sid.and_then(crate::acl::session_principal);
    // The session's own box, else a private one keyed on whoever this
    // is — a cron job carries no conversation, and an A2A turn now
    // binds its PEER's principal (a2a_server, since 2026-09-11), so
    // the fallback is cron and the local owner surfaces only (below).
    let own_box = || {
        sid.and_then(crate::sandboxes::session_sandbox)
            .unwrap_or_else(|| {
                let subject = principal
                    .as_deref()
                    .map(|p| p.subject.clone())
                    .unwrap_or_else(|| "local:owner".to_string());
                crate::sandboxes::normalize(&format!("dm:{subject}"))
            })
    };
    let named = |name: String| {
        let key = key_for(&name);
        remember_label(&key, &name);
        Zone::Sandbox { key, name }
    };
    match principal.as_deref() {
        // Below admin: sandboxed, always. A name is honored only if
        // the tool layer already checked they may reach it.
        Some(p) if p.role < crate::acl::Role::Admin => match arg {
            ZoneArg::Named(n) if may_reach(p, &n) => named(crate::sandboxes::normalize(&n)),
            _ => named(own_box()),
        },
        // Admin and owner over a connector: the sandbox by default,
        // with an explicit way out.
        Some(p) => match arg {
            ZoneArg::Host => Zone::Host,
            ZoneArg::Named(n) if may_reach(p, &n) => named(crate::sandboxes::normalize(&n)),
            ZoneArg::Own => named(own_box()),
            ZoneArg::Unset | ZoneArg::Named(_) => {
                if admin_default_is_sandbox() {
                    named(own_box())
                } else {
                    Zone::Host
                }
            }
        },
        // No principal: a local owner surface (the desktop, the TUI,
        // the REPL, the bare CLI) plus cron. A2A is NOT here any more
        // — since 2026-09-11 an A2A turn binds the peer's principal
        // (a2a_server.rs, re-bound per task on purpose). The human at
        // that keyboard IS the owner, and
        // fencing them out of their own machine by default is not a
        // security posture, it is a bug — so this branch keeps the
        // host default that `[sandbox] local_default_zone` can flip
        // for an operator who wants the fence everywhere.
        None => match arg {
            ZoneArg::Host => Zone::Host,
            ZoneArg::Own => named(own_box()),
            ZoneArg::Named(n) => named(crate::sandboxes::normalize(&n)),
            ZoneArg::Unset => {
                if local_default_is_sandbox() {
                    named(own_box())
                } else {
                    Zone::Host
                }
            }
        },
    }
}

/// Whether `[sandbox] admin_default_zone` puts admins in a box.
pub fn admin_default_is_sandbox() -> bool {
    let raw = if crate::config::config_file_present() {
        crate::config::live().sandbox.admin_default_zone.clone()
    } else {
        crate::config::SandboxSection::current().admin_default_zone
    };
    !matches!(raw.trim().to_ascii_lowercase().as_str(), "host" | "none")
}

/// Whether `[sandbox] local_default_zone` puts the LOCAL surfaces in a
/// box — the desktop, the TUI, the REPL and the bare CLI, which carry
/// no principal at all.
///
/// Defaults to `"host"`, and that default is a deliberate exception to
/// "fail closed": the only way to reach those surfaces is to already
/// be at this machine's keyboard, and an agent that cannot touch the
/// checkout it was asked to edit is not sandboxed, it is broken. An
/// operator who wants the fence everywhere sets `"sandbox"` and pays
/// for it with an explicit `host: true` per call.
pub fn local_default_is_sandbox() -> bool {
    let raw = if crate::config::config_file_present() {
        crate::config::live().sandbox.local_default_zone.clone()
    } else {
        crate::config::SandboxSection::current().local_default_zone
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "sandbox" | "sandboxed" | "user" | "env"
    )
}

/// May this principal name that box? Answered without a
/// `SenderContext` — the zone decision happens deep in a tool call,
/// where the conversation is long gone — so it consults the reachable
/// set the gateway already resolved for the session, plus the
/// admin-reaches-everything rule.
fn may_reach(p: &crate::acl::Principal, name: &str) -> bool {
    crate::sandboxes::may_use_session(p, name)
}

/// Prefix a sandbox-zone tool result so the model can SEE where the
/// call ran — awareness without authority.
pub fn annotate(name: &str, body: String) -> String {
    format!("[env: sandbox {name}]\n{body}")
}

/// The denial for a box the caller may not reach. A tool result, like
/// every other ACL refusal, so the model reads it and adapts instead
/// of trying the next name.
pub fn deny_named_sandbox(name: &str, p: &crate::acl::Principal) -> String {
    format!(
        "error: access denied — {} may not use the sandbox `{name}`. Do not retry with another name; run without a `sandbox` argument to use your own.",
        p.label()
    )
}

// ── keys, paths, container path semantics ─────────────────────────────

/// Stable env key for a sandbox NAME: readable prefix (sanitized,
/// ≤24 chars) + 8-hex hash of (name, veles home, home incarnation).
///
/// Keyed on the name and not on a principal since 2026-09-09, which is
/// what makes a shared box shared: everyone routed to
/// `chan:irc/libera/#veles` hashes to the same key, so they get the
/// same container and the same `/work`.
///
/// The INCARNATION is the subtle part: a home that is deleted and
/// rebuilt at the SAME path (the live suite does exactly this between
/// phases) would otherwise keep hitting a still-running container
/// whose /work bind points at the DELETED directory inode — every
/// "persistent" write vanishing into a dead mount. A fresh
/// incarnation mints a fresh key, so the container is recreated
/// against the new inode and the stale one is left for the sweeper.
pub fn key_for(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut san: String = name
        .chars()
        .take(24)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    while san.ends_with('_') {
        san.pop();
    }
    if san.is_empty() {
        san = "anon".into();
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        name,
        crate::config::veles_home().display().to_string(),
        incarnation(),
    )
        .hash(&mut h);
    format!("{san}-{:08x}", h.finish() as u32)
}

/// Per-home-generation id for env keys (see [`key_for`]). Created on
/// first use; a wiped home mints a new one.
fn incarnation() -> String {
    let path = crate::config::veles_home().join(".veles-env-incarnation");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    let id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let _ = std::fs::write(&path, &id);
    id
}

/// The envs root on the host: `<veles home>/user-envs/`.
pub fn envs_root() -> PathBuf {
    crate::config::veles_home().join("user-envs")
}

/// Host workspace dir for a key (the thing bind-mounted at /work).
pub fn workdir_for(key: &str) -> PathBuf {
    envs_root().join(key).join("work")
}

/// Container name for a key.
pub fn container_name(key: &str) -> String {
    format!("{NAME_PREFIX}{key}")
}

/// Is podman driving a service on the other side of a socket? Podman's
/// own `CONTAINER_HOST` makes every podman command remote — the VPS's
/// gateway container drives the sandbox user's podman that way
/// (deploy/vps). A path on THIS side then means nothing to the service:
/// an env's /work is a named volume ([`volume_for`]) and files travel
/// by `podman cp` and `podman exec`, never through a bind mount.
pub fn podman_remote() -> bool {
    std::env::var("CONTAINER_HOST").is_ok_and(|v| !v.trim().is_empty())
}

/// What makes this node's boxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runtime {
    /// podman on this machine; /work is a bind mount of the host workdir.
    Podman,
    /// podman driving a service over a socket ([`podman_remote`]): /work
    /// is a named volume, files travel by `podman cp`/`podman exec`.
    PodmanRemote,
    /// The phone's userland package, chosen by `[sandbox] backend =
    /// "userland"` (PLAN-phone-userland.md T3 design item 4): each box is
    /// a proot guest of its own, the userland's `files/sandboxes/<key>`,
    /// made from a copy of its rootfs on first use and reached through
    /// this ulsh. The gateway cannot see the userland's files at all, so
    /// files and background shells go by exec, as under a remote podman.
    /// A separation of files, not a fence: every guest runs as the
    /// userland app's uid with the phone's network ([`section_is_fenced`]).
    Userland(PathBuf),
    /// Nothing here can make one.
    None,
}

/// [`runtime`] from what it depends on: the configured backend, a remote
/// podman, a podman on PATH, and the ulsh the userland answers through.
/// The userland is never a fallback: the box it makes is a weaker fence
/// than podman's, so it is used where the config says so.
pub fn runtime_for(backend: &str, remote: bool, podman: bool, ulsh: Option<PathBuf>) -> Runtime {
    if backend.trim() == "userland" {
        return ulsh.map(Runtime::Userland).unwrap_or(Runtime::None);
    }
    if remote {
        Runtime::PodmanRemote
    } else if podman {
        Runtime::Podman
    } else {
        Runtime::None
    }
}

/// The `[sandbox]` section as the live config has it.
fn sandbox_section() -> crate::config::SandboxSection {
    if crate::config::config_file_present() {
        crate::config::live().sandbox.clone()
    } else {
        crate::config::SandboxSection::current()
    }
}

/// What makes this node's boxes now.
pub fn runtime() -> Runtime {
    runtime_for(
        &crate::sandbox::effective_backend(&sandbox_section()).name,
        podman_remote(),
        crate::shell::which(&podman_bin()).is_some(),
        crate::userland::ulsh(),
    )
}

/// Whether a box's files and background shells are reached by exec
/// alone: a remote podman's (no bind mount on this side) and the
/// userland's (another app's storage).
pub fn polls_by_exec() -> bool {
    matches!(runtime(), Runtime::PodmanRemote | Runtime::Userland(_))
}

/// Can this node make a box at all: a podman to run, a remote one to
/// talk to, or (when the config says so) the userland package? The
/// phone has no podman of either kind.
pub fn runtime_available() -> bool {
    runtime() != Runtime::None
}

/// What a sandboxed call says on a node that cannot make a box.
pub fn no_runtime_error() -> String {
    if sandbox_section().backend.trim() == "userland" {
        return "this node's boxes are the userland's ([sandbox] backend = \"userland\"), but \
                the userland package does not answer: the engine's shell is not ulsh, so \
                Veles userland is not installed, is stopped, or failed the gateway's probe \
                (its words are in gateway.log; docs/phone.md \"The userland package\")"
            .to_string();
    }
    format!(
        "this node has no sandbox: no container runtime (`{}` is not here). Set [cluster] \
         sandbox_home = \"<node>\" in its config.toml to run its sandboxed calls on a node \
         that has one",
        podman_bin()
    )
}

/// The named volume that is an env's /work under a remote podman.
pub fn volume_for(key: &str) -> String {
    format!("veles-env-{key}")
}

/// What an env's /work is mounted FROM: the host workdir, or under a
/// remote podman the env's named volume.
fn work_source(key: &str, workdir: &Path) -> String {
    if podman_remote() {
        volume_for(key)
    } else {
        workdir.display().to_string()
    }
}

/// Resolve a model-supplied path against CONTAINER semantics: `~`
/// expands to `/work` (the env's HOME), relative paths join onto
/// `/work` (the exec cwd), absolute paths stay absolute — and point
/// into the CONTAINER's namespace, which is the entire point: a
/// sub-admin reading `/etc/passwd` gets the container's decoy copy.
pub fn container_resolve(path: &str) -> String {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        format!("{CONTAINER_WORK}/{rest}")
    } else if path == "~" {
        CONTAINER_WORK.to_string()
    } else {
        path.to_string()
    };
    let p = PathBuf::from(&expanded);
    if p.is_absolute() {
        expanded
    } else {
        format!("{CONTAINER_WORK}/{expanded}")
    }
}

// ── config + command builders (pure; pinned by tests) ─────────────────

/// What network namespace an env container gets.
///
/// Three states rather than a bool because the old bool could only
/// choose between "no network" and `--network host` — and `host` shares
/// the HOST's namespace, which put every routed caller on the loopback
/// that `desktop_api` trusts for being unreachable. The middle answer
/// (a namespace of its own, with outbound) is the one that was missing,
/// and it is now the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvNet {
    /// No `--network` flag: podman rootless picks its own default
    /// (pasta) — outbound works, the host's 127.0.0.1 is not ours.
    Isolated,
    /// `--network none`.
    None,
    /// `--network host` — the host's namespace, loopback included.
    Host,
}

impl EnvNet {
    /// Parse the config spelling. Unknown values warn and fall back to
    /// `Isolated`: a typo must not silently widen the fence.
    pub fn parse(s: &str) -> EnvNet {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "false" => EnvNet::None,
            "host" => EnvNet::Host,
            "isolated" | "private" | "default" => EnvNet::Isolated,
            other => {
                tracing::warn!(
                    "[sandbox] user_env_network = {other:?} is not isolated|none|host \
                     — using isolated"
                );
                EnvNet::Isolated
            }
        }
    }

    /// The config spelling of this posture — the word an operator would
    /// type, so a readout that uses it can be checked against the file.
    pub fn spelling(&self) -> &'static str {
        match self {
            EnvNet::Isolated => "isolated",
            EnvNet::None => "none",
            EnvNet::Host => "host",
        }
    }
}

/// The `[sandbox]` slice an env container inherits.
#[derive(Debug, Clone)]
pub struct EnvCfg {
    pub image: String,
    pub memory_mb: u64,
    pub net: EnvNet,
    pub drop_caps: bool,
}

impl EnvCfg {
    pub fn from_section(s: &crate::config::SandboxSection) -> Self {
        Self {
            image: s.image.clone(),
            memory_mb: s.memory_mb,
            // `network = false` is the untrusted posture and wins
            // outright — the three-way knob only refines what
            // `network = true` is allowed to mean.
            net: if s.network {
                EnvNet::parse(&s.user_env_network)
            } else {
                EnvNet::None
            },
            drop_caps: s.capabilities.eq_ignore_ascii_case("drop"),
        }
    }

    pub fn current() -> Self {
        Self::from_section(&crate::config::SandboxSection::current())
    }
}

fn podman_bin() -> String {
    std::env::var("VELES_PODMAN_BIN").unwrap_or_else(|_| "podman".into())
}

/// `podman create` argv for an env container. `sleep infinity` is the
/// keep-alive entrypoint — the container is a long-lived workspace,
/// work enters via `exec`, never via a fresh `run`.
pub fn build_create_command(
    bin: &str,
    name: &str,
    work_host: &Path,
    cfg: &EnvCfg,
) -> std::process::Command {
    build_create_command_src(bin, name, &work_host.display().to_string(), cfg)
}

/// [`build_create_command`] with /work mounted from `work_src`: a host
/// path, or a named volume (a remote podman, [`podman_remote`]).
pub fn build_create_command_src(
    bin: &str,
    name: &str,
    work_src: &str,
    cfg: &EnvCfg,
) -> std::process::Command {
    build_create_command_owned(bin, name, work_src, cfg, None)
}

fn build_create_command_owned(
    bin: &str,
    name: &str,
    work_src: &str,
    cfg: &EnvCfg,
    owner: Option<&EnvOwner>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("create")
        .arg("--pull=never")
        .arg("--name")
        .arg(name);
    if let Some(owner) = owner {
        for label in owner.labels() {
            cmd.arg("--label").arg(label);
        }
    }
    // ISOLATED passes no flag at all, which is not the same as passing
    // a default: podman rootless then builds its own namespace (pasta),
    // and that is the whole point — the container gets outbound without
    // getting the host's loopback.
    match cfg.net {
        EnvNet::Isolated => {}
        EnvNet::None => {
            cmd.arg("--network").arg("none");
        }
        EnvNet::Host => {
            cmd.arg("--network").arg("host");
        }
    }
    cmd.arg("--memory")
        .arg(format!("{}m", cfg.memory_mb))
        .arg("--memory-swap")
        .arg(format!("{}m", cfg.memory_mb));
    if cfg.drop_caps {
        cmd.arg("--cap-drop").arg("ALL");
    }
    cmd.arg("--security-opt")
        .arg("no-new-privileges")
        // A fixed hostname. It mattered most under `--network host`,
        // which shares the host's UTS namespace and made `hostname`
        // inside an env name the HOST machine — indistinguishable, by
        // observation, from host-side execution. It is kept under every
        // posture so that identification does not depend on which one
        // is configured.
        .arg("--hostname")
        .arg("veles-user-env")
        .arg("-v")
        .arg(format!("{work_src}:{CONTAINER_WORK}"))
        .arg("-w")
        .arg(CONTAINER_WORK)
        .arg("-e")
        .arg(format!("HOME={CONTAINER_WORK}"))
        .arg("-e")
        .arg(format!("TMPDIR={CONTAINER_WORK}"))
        .arg(&cfg.image)
        .arg("sleep")
        .arg("infinity");
    cmd
}

/// `podman exec` argv. Env pairs are FRAMEWORK-constructed only (HOME,
/// staging vars) — the gateway's environment never crosses, which is
/// how the F3 strip holds here by construction.
pub fn build_exec_command(
    bin: &str,
    name: &str,
    cwd: &str,
    env: &[(&str, String)],
    argv: &[String],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("exec").arg("-w").arg(cwd);
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(name);
    for a in argv {
        cmd.arg(a);
    }
    cmd
}

/// `podman cp` argv (either direction — caller composes the
/// `name:path` side).
pub fn build_cp_command(bin: &str, from: &str, to: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("cp").arg(from).arg(to);
    cmd
}

// ── the manager ───────────────────────────────────────────────────────

/// Result of one container exec.
#[derive(Debug, Default, Clone)]
pub struct ExecOut {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// True when the CLIENT-side deadline fired (the in-container
    /// `timeout` wrapper usually wins first and shows as exit 124).
    pub timed_out: bool,
}

fn key_locks() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static L: std::sync::OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_for(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    key_locks()
        .lock()
        .unwrap()
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// How many container `exec`s one box runs at a time, live from
/// `[sandbox] max_concurrent_exec`. 0 is unbounded.
/// Is a user-env container a real fence, or has it been opened up?
///
/// The approval gate substitutes human judgement for a missing
/// boundary. When the boundary is actually there, the gate is asking
/// somebody to approve `rm -rf` inside a container that is thrown away
/// — which is the fastest way to teach a channel that the prompts are
/// noise. So: a fenced box skips the gate (and says so).
///
/// Fenced means all of:
///
/// - a real container backend for user envs (podman: the `[sandbox]
///   backend` knob is otherwise the OTHER sandbox, `sandbox.rs`, and
///   only `userland` moves user envs, onto a weaker fence — below),
/// - a network namespace that is not the host's, so the box cannot
///   reach loopback services that trust being unreachable,
/// - capabilities dropped,
/// - no host mounts. `[[sandbox.mounts]]` and `extra_args` are not
///   applied to user envs today, which is why this reads them anyway:
///   the day they are, this predicate must already know.
///
/// A userland box (`[sandbox] backend = "userland"`) is never fenced:
/// proot has no network namespace (the guest is on the phone's loopback),
/// no capabilities to drop, and is a ptrace tracer, not a boundary — so
/// its commands keep the approval gate.
pub fn env_is_fenced() -> bool {
    section_is_fenced(&sandbox_section())
}

/// [`env_is_fenced`] for a given section.
pub fn section_is_fenced(s: &crate::config::SandboxSection) -> bool {
    // The effective backend, not the key: on a phone the default is
    // `auto`, and it makes the boxes the userland's.
    if crate::sandbox::effective_backend(s).name.trim() == "userland" {
        return false;
    }
    let cfg = EnvCfg::from_section(s);
    cfg.net != EnvNet::Host && cfg.drop_caps && s.mounts.is_empty() && s.extra_args.is_empty()
}

pub fn max_concurrent_exec() -> u32 {
    if crate::config::config_file_present() {
        crate::config::live().sandbox.max_concurrent_exec
    } else {
        crate::config::SandboxSection::current().max_concurrent_exec
    }
}

/// How long a call waits for one of its box's exec slots before it is
/// refused. Long enough to ride out a neighbour's `cargo build`-shaped
/// command, short enough that the caller gets an answer in the same
/// turn they asked.
const EXEC_QUEUE_WAIT_SECS: u64 = 30;

/// One in-flight exec on one box, counted. Dropping it frees the slot and
/// wakes whoever is waiting for it.
///
/// A guard rather than a counter the caller decrements, because the
/// caller is `exec_argv` and the slot is held across a container exec
/// that can fail in a dozen places: a leaked slot is permanent and
/// invisible, and the box it belongs to would slowly stop answering.
#[derive(Debug)]
struct ExecSlot {
    key: String,
}

impl Drop for ExecSlot {
    fn drop(&mut self) {
        let mut m = exec_slots().lock().unwrap();
        if let Some(slot) = m.get_mut(&self.key) {
            slot.0 = slot.0.saturating_sub(1);
            // ONE waiter: the woken pass re-reads the live limit and
            // either takes the slot or waits again, and a release wakes
            // the next one. Broadcasting would just stampede every box
            // sharer through a check that admits one of them.
            slot.1.notify_one();
        }
    }
}

/// The per-box exec slots: how many are in flight, and who to wake when
/// one frees. Per BOX because the box is the resource being protected
/// (`[sandbox] max_concurrent_exec`), and counted rather than semaphored
/// so admission can be compared against the limit in force NOW.
///
/// The semaphore this replaced was rebuilt whenever the limit changed,
/// and permits held against the old one were invisible to the new one —
/// so lowering the knob under a busy box admitted `limit` MORE execs on
/// top of the ones already running. That was documented as the price of
/// a live knob. It is not: counting holders costs one integer per box,
/// and the price was paid at exactly the moment somebody was trying to
/// slow a box down.
///
/// Entries are never removed, not even at zero. A waiter holds an
/// `Arc<Notify>` cloned out of its box's entry; dropping the entry would
/// leave it waiting on a notification that the next admission — which
/// creates a FRESH notify — can never send, and it would sit out its
/// whole budget beside an idle box.
#[allow(clippy::type_complexity)]
fn exec_slots() -> &'static Mutex<HashMap<String, (usize, Arc<tokio::sync::Notify>)>> {
    static L: std::sync::OnceLock<Mutex<HashMap<String, (usize, Arc<tokio::sync::Notify>)>>> =
        std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take one of this box's exec slots, or refuse and say why.
///
/// `Ok(None)` means the bound is off, not that a slot was skipped.
async fn acquire_exec_slot(key: &str) -> anyhow::Result<Option<ExecSlot>> {
    acquire_exec_slot_within(key, Duration::from_secs(EXEC_QUEUE_WAIT_SECS)).await
}

async fn acquire_exec_slot_within(key: &str, wait: Duration) -> anyhow::Result<Option<ExecSlot>> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // Read live on EVERY pass: the knob is the reason this waits at
        // all, and an operator raising it during a stall should not have
        // to wait for the stalled calls to time out first.
        let limit = max_concurrent_exec();
        if limit == 0 {
            return Ok(None);
        }
        let notify = {
            let mut m = exec_slots().lock().unwrap();
            let slot = m
                .entry(key.to_string())
                .or_insert_with(|| (0, Arc::new(tokio::sync::Notify::new())));
            if slot.0 < limit as usize {
                slot.0 += 1;
                return Ok(Some(ExecSlot {
                    key: key.to_string(),
                }));
            }
            Arc::clone(&slot.1)
        };
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            anyhow::bail!(
                "sandbox `{}` is already running {} commands and this call waited {}s \
for a slot — someone else sharing this box is using it. Retry, or raise \
`[sandbox] max_concurrent_exec` (currently {}).",
                label_for(key).unwrap_or_else(|| key.to_string()),
                limit,
                wait.as_secs(),
                limit
            );
        }
        // `notify_one` stores a permit when nobody is registered, so a
        // release landing between the check above and this await is not
        // lost — the next pass simply finds the slot free.
        let _ = tokio::time::timeout(left, notify.notified()).await;
    }
}

fn last_use() -> &'static Mutex<HashMap<String, std::time::Instant>> {
    static L: std::sync::OnceLock<Mutex<HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn touch(key: &str) {
    last_use()
        .lock()
        .unwrap()
        .insert(key.to_string(), std::time::Instant::now());
}

/// True when `p` (after symlink resolution) stays under `root`.
/// The one guard every host-side workspace touch goes through.
fn stays_under(root: &Path, p: &Path) -> bool {
    match (root.canonicalize(), p.canonicalize()) {
        (Ok(r), Ok(c)) => c.starts_with(&r),
        _ => false,
    }
}

/// Prepare a framework-owned directory inside the workspace: remove
/// whatever is there (a pre-planted symlink to the host `/etc` loses
/// its audience), recreate, and prove the result stays inside the
/// workspace. `Err` names the path when the guard trips. Every
/// ANCESTOR component is cleaned too — a symlink at `.veles-run`
/// itself would otherwise redirect the framework's own writes into
/// `/etc` via the bind mount.
fn fresh_stage_dir(workdir: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let mut cur = workdir.to_path_buf();
    for comp in Path::new(rel).components() {
        cur = cur.join(comp.as_os_str());
        if let Ok(m) = std::fs::symlink_metadata(&cur) {
            if m.file_type().is_symlink() {
                std::fs::remove_file(&cur)?;
            }
        }
    }
    let dir = workdir.join(rel);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    if !stays_under(workdir, &dir) {
        anyhow::bail!("sandbox staging path {rel} escapes the workspace — refusing");
    }
    Ok(dir)
}

/// Put framework-written files at `/work/<rel>` in the env, as a fresh
/// directory. With a bind mount they are written through it into a
/// guard-checked stage dir, which is returned for the caller to remove.
/// Under a remote podman ([`podman_remote`]) they are written to a local
/// staging dir and copied in: `podman cp` resolves the path INSIDE the
/// container, so a symlink planted in the workspace cannot point it at
/// this machine. The caller removes `/work/<rel>` inside (`None`). In the
/// userland every file goes in on a guest command's stdin, the path
/// resolved inside the guest for the same reason.
async fn stage_into(
    key: &str,
    workdir: &Path,
    rel: &str,
    files: &[(&str, &[u8])],
) -> anyhow::Result<Option<PathBuf>> {
    if let Runtime::Userland(ulsh) = runtime() {
        let target = format!("{CONTAINER_WORK}/{rel}");
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "rm -rf \"$1\" && mkdir -p \"$1\"".to_string(),
            "veles-stage".to_string(),
            target.clone(),
        ];
        let out = exec_argv(key, &argv, 30).await?;
        if out.exit_code != 0 {
            anyhow::bail!("staging {rel}: {}", out.stderr.trim());
        }
        for (name, bytes) in files {
            userland_write(&ulsh, key, &format!("{target}/{name}"), bytes).await?;
        }
        return Ok(None);
    }
    if !podman_remote() {
        let dir = fresh_stage_dir(workdir, rel)?;
        for (name, bytes) in files {
            std::fs::write(dir.join(name), bytes)?;
        }
        return Ok(Some(dir));
    }
    let stage = staging_dir()?;
    let local = stage.join("d");
    std::fs::create_dir_all(&local)?;
    let res = async {
        for (name, bytes) in files {
            std::fs::write(local.join(name), bytes)?;
        }
        let target = format!("{CONTAINER_WORK}/{rel}");
        let parent = Path::new(&target)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| CONTAINER_WORK.to_string());
        // Whatever is already there goes first (a background task id is
        // reused after a restart): the copy must CREATE the path, which
        // is how a directory lands as itself and not inside it.
        for argv in [
            vec!["rm".to_string(), "-rf".to_string(), target.clone()],
            vec!["mkdir".to_string(), "-p".to_string(), parent],
        ] {
            let out = exec_argv(key, &argv, 30).await?;
            if out.exit_code != 0 {
                anyhow::bail!("staging {rel}: {} failed: {}", argv[0], out.stderr.trim());
            }
        }
        let mut cp = tokio::process::Command::from(build_cp_command(
            &podman_bin(),
            &local.display().to_string(),
            &format!("{}:{target}", container_name(key)),
        ));
        let out = podman_output(&mut cp).await?;
        if !out.status.success() {
            anyhow::bail!(
                "podman cp into {rel} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    res.map(|_| None)
}

/// Remove what [`stage_into`] put in, wherever it put it.
async fn unstage(key: &str, rel: &str, host_dir: Option<PathBuf>) {
    match host_dir {
        Some(d) => {
            let _ = std::fs::remove_dir_all(d);
        }
        None => {
            let argv = vec![
                "rm".to_string(),
                "-rf".to_string(),
                format!("{CONTAINER_WORK}/{rel}"),
            ];
            let _ = exec_argv(key, &argv, 30).await;
        }
    }
}

/// Let the box `key` use at least `mb` of memory, for the rest of its
/// life. A box is made with `[sandbox] memory_mb` (256 MB by default),
/// which a drawn Chrome does not fit in; the guest browser raises its own
/// box when it opens a page (PLAN-challenge-relay.md D6), so only the
/// boxes that run one hold more. A box rebuilt later starts from the
/// config again.
pub async fn raise_memory(key: &str, mb: u64) -> anyhow::Result<()> {
    if EnvCfg::current().memory_mb >= mb {
        return Ok(());
    }
    // A userland box has no cgroup, so no limit to raise.
    if matches!(runtime(), Runtime::Userland(_)) {
        return Ok(());
    }
    let limit = format!("{mb}m");
    let mut cmd = tokio::process::Command::new(podman_bin());
    cmd.args(["update", "--memory", &limit, "--memory-swap", &limit])
        .arg(container_name(key));
    let out = podman_output(&mut cmd).await?;
    if !out.status.success() {
        anyhow::bail!(
            "podman update {}: {}",
            container_name(key),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn podman_output(cmd: &mut tokio::process::Command) -> anyhow::Result<std::process::Output> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    Ok(cmd.output().await?)
}

// ── the userland's boxes (Runtime::Userland) ─────────────────────────

/// One `ulsh --exec` in the box `key` (`crate::userland::command`: no
/// environment of the engine's crosses, HOME is /work), its output
/// whole, or `None` when `timeout_secs` (0: none) passed first. ulsh is
/// then killed, and its hang-up is the broker's signal to kill the
/// guest's whole process group — the timeout reaches the guest.
async fn userland_output(
    ulsh: &Path,
    key: &str,
    cwd: &str,
    argv: &[String],
    env: &[(&str, String)],
    stdin: Option<&[u8]>,
    timeout_secs: u64,
) -> anyhow::Result<Option<std::process::Output>> {
    let call = crate::userland::GuestCall {
        sandbox: Some(key),
        cwd,
        home: Some(CONTAINER_WORK),
        env,
        argv,
    };
    let mut cmd = tokio::process::Command::from(crate::userland::command(ulsh, &call, |k| {
        std::env::var(k).ok()
    }));
    cmd.stdin(if stdin.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start ulsh ({}): {e}", ulsh.display()))?;
    let feed = child.stdin.take();
    let bytes = stdin.map(<[u8]>::to_vec);
    let run = async move {
        let write = async move {
            if let (Some(mut w), Some(b)) = (feed, bytes) {
                use tokio::io::AsyncWriteExt;
                let _ = w.write_all(&b).await;
                // Dropped here: the guest's stdin ends.
            }
        };
        let (_, out) = tokio::join!(write, child.wait_with_output());
        out
    };
    if timeout_secs == 0 {
        return Ok(Some(run.await?));
    }
    match tokio::time::timeout(Duration::from_secs(timeout_secs), run).await {
        Ok(out) => Ok(Some(out?)),
        Err(_) => Ok(None),
    }
}

/// `bytes` to `path` in the box, on a guest command's stdin (the path
/// resolved inside the guest, like `podman cp`'s).
async fn userland_write(ulsh: &Path, key: &str, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "cat > \"$1\"".to_string(),
        "veles-write".to_string(),
        path.to_string(),
    ];
    let out = userland_output(ulsh, key, CONTAINER_WORK, &argv, &[], Some(bytes), 120)
        .await?
        .ok_or_else(|| anyhow::anyhow!("writing {path} in the box took over 120s"))?;
    if !out.status.success() {
        anyhow::bail!(
            "writing {path} in the box failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The boxes this process has opened in the userland (made, /work made,
/// the marker checked), so the first-use command runs once per box.
fn userland_opened() -> &'static Mutex<std::collections::HashSet<String>> {
    static S: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// A userland box's first command in this process. The broker makes the
/// box from a copy of the rootfs when the name is new (a second or two on
/// the phone), `/work` is made, and the guest says which box it is in
/// (the broker's marker, veles-userland `proto::SANDBOX_MARKER`). A
/// broker older than sandboxes runs the request in its rootfs, without
/// the marker — and the box is refused before anything else runs there.
pub const USERLAND_FIRST: &str = "mkdir -p /work && printf %s \"$VELES_SANDBOX\"";

/// Whether a box's answer to [`USERLAND_FIRST`] says it ran in box `key`.
pub fn userland_box_answer(key: &str, ok: bool, stdout: &str, stderr: &str) -> anyhow::Result<()> {
    if !ok {
        anyhow::bail!("the userland could not open box {key}: {}", stderr.trim());
    }
    let said = stdout.trim();
    if said != key {
        anyhow::bail!(
            "the userland ran box {key}'s first command somewhere else (the guest says it is \
             in {said:?}): this Veles userland is older than this veles and keeps no \
             sandboxes. Install the userland APK from the same build (docs/phone.md \"The \
             userland package\"); nothing more runs there until then"
        );
    }
    Ok(())
}

/// [`ensure`] for a userland box. Returns the gateway-side record dir's
/// `work` path, which no host-side read may use (the box's files are the
/// userland app's; [`polls_by_exec`] is true).
async fn ensure_userland(ulsh: &Path, key: &str) -> anyhow::Result<PathBuf> {
    touch(key);
    let workdir = workdir_for(key);
    if userland_opened().lock_ok().contains(key) {
        return Ok(workdir);
    }
    let lock = lock_for(key);
    let _g = lock.lock().await;
    if userland_opened().lock_ok().contains(key) {
        return Ok(workdir);
    }
    if EnvCfg::current().net == EnvNet::None {
        anyhow::bail!(
            "[sandbox] asks for boxes with no network (network = false or user_env_network = \
             \"none\"), and the userland cannot take it away: proot has no network namespace. \
             Use a podman node for such boxes ([cluster] sandbox_home)"
        );
    }
    // The name beside the key for `.agent sandboxes list`; the box itself
    // is the userland's, out of this app's sight.
    let root = envs_root();
    std::fs::create_dir_all(root.join(key))?;
    if let Some(name) = label_for(key) {
        let _ = std::fs::write(root.join(key).join("name"), name);
    }
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        USERLAND_FIRST.to_string(),
    ];
    let out = userland_output(ulsh, key, "/", &argv, &[], None, 180)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the userland did not open box {key} within 180s"))?;
    userland_box_answer(
        key,
        out.status.success(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    )?;
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::SeqCst) {
        tracing::info!(
            "user env boxes are the userland's proot guests (Veles userland's \
             files/sandboxes/<key>): apart in their files, but on the phone's network and uid, \
             so their commands keep the approval gate"
        );
    }
    userland_opened().lock_ok().insert(key.to_string());
    Ok(workdir)
}

/// Background shells held in userland boxes, by the pid of the ulsh that
/// holds each ([`bg_launch_argv`] says why they are held).
fn userland_bg() -> &'static Mutex<HashMap<u32, tokio::process::Child>> {
    static M: std::sync::OnceLock<Mutex<HashMap<u32, tokio::process::Child>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn inspect_running(name: &str) -> anyhow::Result<bool> {
    Ok(inspect_state(name).await?.is_some_and(|s| s.running))
}

/// Running flag, /work mount source and network mode of a container in
/// ONE inspect. None when the container does not exist.
#[derive(Debug)]
struct ContainerState {
    running: bool,
    work_source: Option<String>,
    net_mode: String,
}

/// Does a live container's network mode match the configured posture?
///
/// This is what makes tightening the posture reach a deployment that is
/// ALREADY RUNNING. A channel box created under `--network host` keeps
/// the host's namespace for as long as the container lives, so without
/// this check an upgrade would close the hole only for boxes created
/// after it — leaving it open for exactly the people who already had
/// one. A mismatch marks the container stale, and `ensure` rebuilds it.
///
/// `Isolated` is spelled as "neither of the two named modes" rather than
/// as a literal: podman reports the rootless default by name (`pasta`
/// here, `slirp4netns` on older installs), and pinning today's spelling
/// would turn a podman upgrade into a rebuild loop.
fn net_mode_matches(mode: &str, want: EnvNet) -> bool {
    let m = mode.trim().to_ascii_lowercase();
    match want {
        EnvNet::Host => m == "host",
        EnvNet::None => m == "none",
        EnvNet::Isolated => m != "host" && m != "none",
    }
}

/// Why an existing container cannot be used as it is, or None when it
/// can (started if stopped). `ensure`'s rule, kept pure so it can be
/// pinned: a /work bound elsewhere and a network mode off the posture
/// rebuild; being stopped does not.
fn rebuild_reason(st: &ContainerState, work_str: &str, want: EnvNet) -> Option<String> {
    if st.work_source.as_deref() != Some(work_str) {
        return Some(format!(
            "/work is bound to {}, not {work_str}",
            st.work_source.as_deref().unwrap_or("nothing")
        ));
    }
    if !net_mode_matches(&st.net_mode, want) {
        return Some(format!(
            "it is on network mode {:?} but the configured posture is {:?}",
            st.net_mode, want
        ));
    }
    None
}

// ── what this host can actually give an Isolated box ──────────────────

/// `Isolated` passes no `--network` flag and trusts podman to build a
/// namespace with outbound in it. That trust has a precondition: a
/// rootless podman needs a rootless network driver — `pasta` or
/// `slirp4netns` — and when neither is installed it still CREATES the
/// container. Nothing errors. The box simply has no outbound, and
/// nothing can be published into it, which makes `isolated`
/// indistinguishable from `none` to everything inside it while
/// `net_mode_matches` reports the configured posture as satisfied.
///
/// This is the honest answer to "does it, on THIS host".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RootlessNet {
    /// `podman info` answered and parsed.
    pub probed: bool,
    /// podman is running rootless (the only case where a driver is
    /// needed — a rooted podman builds a real bridge).
    pub rootless: bool,
    /// The rootless backend podman says it will use.
    pub backend: String,
    /// That backend's executable was found.
    pub driver_present: bool,
}

impl RootlessNet {
    /// Does an `Isolated` box here get outbound?
    ///
    /// An unprobed host answers yes. "We could not ask podman" is not
    /// evidence of a broken fence, and saying otherwise would turn a
    /// missing binary or a slow daemon into a warning about networking —
    /// the wrong subject. The probe's failure mode has to be silence.
    pub fn isolated_has_outbound(&self) -> bool {
        !self.probed || !self.rootless || self.driver_present
    }

    /// The finding in words, for a log line and for `veles doctor`.
    pub fn describe(&self) -> String {
        if !self.probed {
            return "`podman info` did not answer, so the rootless network driver is unknown"
                .to_string();
        }
        if !self.rootless {
            return "podman is rooted — it builds a real bridge, no rootless driver needed"
                .to_string();
        }
        if self.driver_present {
            return format!(
                "rootless with the `{}` driver — outbound works",
                self.backend
            );
        }
        format!(
            "rootless and podman intends to use `{}`, but that helper is not installed \
             (nor is the other of pasta/slirp4netns)",
            if self.backend.is_empty() {
                "a driver it did not name"
            } else {
                &self.backend
            }
        )
    }
}

/// Classify `podman info`'s `.Host` object.
///
/// Split out from the probe so the classification is pinned against
/// captured JSON instead of against whatever the machine running the
/// tests happens to have installed — which on a NixOS host is always
/// "pasta, present", because podman ships its own copy in
/// `libexec/podman/` and `command -v pasta` says MISSING while podman
/// finds it just fine. PATH is not the question; `podman info` is.
///
/// It parses JSON rather than using `--format` templates because the
/// fields that matter are not all reachable as templates:
/// `{{.Host.RootlessNetworkCmd}}` works, but `{{.Host.Pasta.Executable}}`
/// does not exist on `define.HostInfo` at all. Missing helpers are
/// reported as an EMPTY `executable` string, not an absent key.
fn rootless_net_from_info(host: &Value) -> RootlessNet {
    let rootless = host
        .get("security")
        .and_then(|s| s.get("rootless"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let backend = host
        .get("rootlessNetworkCmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // The driver podman intends to use, and the other one as a fallback:
    // a containers.conf that names a helper which is not installed is
    // exactly the case being detected, and podman's own answer about
    // which it picked is not always the one it can honour.
    let present = |name: &str| {
        host.get(name)
            .and_then(|d| d.get("executable"))
            .and_then(Value::as_str)
            .is_some_and(|e| !e.trim().is_empty())
    };
    RootlessNet {
        probed: true,
        rootless,
        driver_present: present("pasta") || present("slirp4netns"),
        backend,
    }
}

static ROOTLESS_NET: tokio::sync::OnceCell<RootlessNet> = tokio::sync::OnceCell::const_new();

/// This host's rootless networking, asked once per process.
///
/// One `podman info` per host, not per box: `ensure` runs on every
/// sandbox operation and the answer cannot change underneath a running
/// gateway, so the cost belongs at first use and the caching is the
/// reason this is safe to call from a hot path.
pub async fn rootless_net_state() -> &'static RootlessNet {
    ROOTLESS_NET.get_or_init(probe_rootless_net).await
}

async fn probe_rootless_net() -> RootlessNet {
    let mut c = tokio::process::Command::new(podman_bin());
    c.arg("info").arg("--format").arg("{{json .Host}}");
    let Ok(out) = podman_output(&mut c).await else {
        return RootlessNet::default();
    };
    if !out.status.success() {
        return RootlessNet::default();
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .map(|host| rootless_net_from_info(&host))
        .unwrap_or_default()
}

async fn inspect_state(name: &str) -> anyhow::Result<Option<ContainerState>> {
    let mut c = tokio::process::Command::new(podman_bin());
    c.arg("inspect")
        .arg("-f")
        .arg("{{.State.Running}} {{.HostConfig.NetworkMode}} {{json .Mounts}}")
        .arg(name);
    let out = podman_output(&mut c).await?;
    if !out.status.success() {
        return Ok(None); // no such container
    }
    let line = String::from_utf8_lossy(&out.stdout).to_string();
    let (run, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
    let (net_mode, mounts) = rest.split_once(' ').unwrap_or((rest, ""));
    let net_mode = net_mode.trim().to_string();
    let running = run.trim() == "true";
    let work_source = serde_json::from_str::<Value>(mounts.trim())
        .ok()
        .and_then(|v| work_mount_of(&v).map(|(_, src)| src));
    Ok(Some(ContainerState {
        running,
        work_source,
        net_mode,
    }))
}

/// A container's /work mount from `podman inspect`'s `.Mounts`:
/// `(is_volume, source)`, where a named volume's source is its NAME —
/// its `Source` is a path on the podman service's host, which under a
/// remote podman is not this machine.
fn work_mount_of(mounts: &Value) -> Option<(bool, String)> {
    let m = mounts
        .as_array()?
        .iter()
        .find(|m| m["Destination"].as_str() == Some(CONTAINER_WORK))?;
    if m["Type"].as_str() == Some("volume") {
        return m["Name"].as_str().map(|n| (true, n.to_string()));
    }
    m["Source"].as_str().map(|s| (false, s.to_string()))
}

/// The /work mount of a container, for GC decisions (see
/// [`work_mount_of`]).
async fn inspect_work_source(name: &str) -> Option<(bool, String)> {
    let mut c = tokio::process::Command::new(podman_bin());
    c.arg("inspect").arg("-f").arg("{{json .Mounts}}").arg(name);
    let out = podman_output(&mut c).await.ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    work_mount_of(&v)
}

/// One inspected container or volume. JSON rather than a whitespace
/// template keeps paths with spaces intact. Both inspect forms are
/// arrays on the supported podman path.
#[derive(Debug)]
struct OwnedResource {
    id: String,
    owner: Option<EnvOwner>,
    // None alone cannot distinguish a pre-label resource from one with
    // a partial/corrupt ownership tuple. The latter must fail closed.
    ownership_incomplete: bool,
    work_mount: Option<(bool, String)>,
}

async fn inspect_owned(bin: &str, kind: &str, name: &str) -> anyhow::Result<Option<OwnedResource>> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(kind).arg("inspect").arg(name);
    let out = podman_output(&mut cmd).await?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr);
        let missing = error.to_ascii_lowercase().contains("no such")
            || error.to_ascii_lowercase().contains("not found");
        if missing {
            return Ok(None);
        }
        anyhow::bail!(
            "cannot inspect {kind} {name}; preserving it: {}",
            error.trim()
        );
    }
    let json: Value = serde_json::from_slice(&out.stdout)?;
    let Some(object) = json.as_array().and_then(|a| a.first()) else {
        return Ok(None);
    };
    let resource = owned_resource_from_inspect(object)
        .ok_or_else(|| anyhow::anyhow!("podman inspect {kind} {name} returned no id"))?;
    Ok(Some(resource))
}

/// Decode the fields the lifecycle gates use from ONE inspect object.
/// This is deliberately the same helper in production and its shape
/// test: `Mounts` is a field of a container object, not that object.
fn owned_resource_from_inspect(object: &Value) -> Option<OwnedResource> {
    let id = object
        .get("Id")
        .or_else(|| object.get("Name"))
        .and_then(Value::as_str)?
        .to_string();
    let labels = object
        .pointer("/Config/Labels")
        .or_else(|| object.get("Labels"));
    let owner = labels.and_then(|labels| {
        let node = labels.get(NODE_LABEL)?.as_str()?.trim();
        let home = labels.get(HOME_LABEL)?.as_str()?.trim();
        let generation = labels.get(INCARNATION_LABEL)?.as_str()?.trim();
        (!node.is_empty() && !home.is_empty() && !generation.is_empty()).then(|| EnvOwner {
            node: node.to_string(),
            home: PathBuf::from(home),
            generation: generation.to_string(),
        })
    });
    let ownership_incomplete = labels.is_some_and(|labels| {
        [NODE_LABEL, HOME_LABEL, INCARNATION_LABEL]
            .iter()
            .any(|key| labels.get(key).is_some())
    }) && owner.is_none();
    Some(OwnedResource {
        id,
        owner,
        ownership_incomplete,
        work_mount: object.get("Mounts").and_then(work_mount_of),
    })
}

/// Delete an older incarnation only after a fresh inspect returns the
/// same id and owner. A renamed/recreated object must not be removed
/// because it happened to appear in an earlier listing.
async fn remove_previous(
    bin: &str,
    kind: &str,
    resource: &OwnedResource,
    current: &EnvOwner,
) -> anyhow::Result<bool> {
    let Some(owner) = &resource.owner else {
        return Ok(false);
    };
    if !is_previous_incarnation(owner, current) {
        return Ok(false);
    }
    let Some(latest) = inspect_owned(bin, kind, &resource.id).await? else {
        return Ok(false);
    };
    if latest.id != resource.id || latest.owner.as_ref() != Some(owner) {
        return Ok(false);
    }
    let mut rm = tokio::process::Command::new(bin);
    if kind == "volume" {
        rm.arg("volume");
    }
    rm.arg("rm").arg("-f").arg(&resource.id);
    let out = podman_output(&mut rm).await?;
    if !out.status.success() {
        anyhow::bail!(
            "removing old user-env {kind} {} failed: {}",
            resource.id,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    tracing::info!(
        "user env previous incarnation {kind} removed: {}",
        resource.id
    );
    Ok(true)
}

/// Reap old containers and remote volumes of THIS node/home generation.
/// The volume pass is necessary when its container was already removed.
async fn reap_previous_incarnations(bin: &str) {
    let Some(current) = current_owner() else {
        return;
    };
    for (list, kind, names) in [
        (["ps", "-a"], "container", "{{.Names}}"),
        (["volume", "ls"], "volume", "{{.Name}}"),
    ] {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(list).args([
            "--filter",
            &format!("label={NODE_LABEL}={}", current.node),
            "--filter",
            &format!("label={HOME_LABEL}={}", current.home.display()),
            "--format",
            names,
        ]);
        let Ok(out) = podman_output(&mut cmd).await else {
            continue;
        };
        if !out.status.success() {
            tracing::warn!(
                "user env old-generation {kind} listing failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            continue;
        }
        for name in String::from_utf8_lossy(&out.stdout).lines().map(str::trim) {
            if name.is_empty()
                || (kind == "container" && !name.starts_with(NAME_PREFIX))
                || (kind == "volume" && !name.starts_with(VOLUME_PREFIX))
            {
                continue;
            }
            let resource = match inspect_owned(bin, kind, name).await {
                Ok(Some(resource)) => resource,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        "user env old-generation cleanup cannot inspect {kind} {name}; preserving it: {error:#}"
                    );
                    continue;
                }
            };
            if let Err(e) = remove_previous(bin, kind, &resource, &current).await {
                tracing::warn!("user env old-generation cleanup: {e:#}");
            }
        }
    }
}

/// Set once a test takes `testenv`'s lock: that process's envs are
/// offline, exactly as under this crate's own unit tests (`cfg!(test)`).
///
/// `cfg!(test)` is false where veles-core is a DEPENDENCY, and that
/// covered more than the live suites it was meant for: the gateway's
/// own unit tests (veles-frontends) route `irc:alice`/`irc:someone`
/// below owner, into the sandbox, and made real boxes. An env's key
/// hashes the VELES_HOME, a scratch one per test, so every gate run
/// left three new `veles-user-env-dm_irc_*` containers behind: 244 by
/// 2026-10-09, the newest still running `sleep infinity`. The live
/// suites (acl_live.rs, user_env_live.rs) set their homes without
/// `testenv` and keep the real fence.
static TEST_OFFLINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[doc(hidden)]
pub fn set_test_offline() {
    TEST_OFFLINE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Create-if-missing, start-if-stopped, guard the workspace. Returns
/// the HOST workdir path. Idempotent, per-key locked.
pub async fn ensure(key: &str) -> anyhow::Result<PathBuf> {
    // Tests run with envs offline: the zone routing, keys, argv and
    // guards are all pure and pinned without spawning podman, and a
    // test must never touch the developer's real podman storage. This
    // crate's unit tests by `cfg!(test)`, other crates' through
    // `testenv` ([`set_test_offline`]); the IRC live suite and every
    // production binary get the real fence.
    if cfg!(test) || TEST_OFFLINE.load(std::sync::atomic::Ordering::Relaxed) {
        anyhow::bail!(
            "user envs are offline under tests — the live IRC suite \
(acl_live.rs) proves the real fence"
        );
    }
    // Not podman's own "No such file or directory": say what is missing
    // and what fixes it.
    match runtime() {
        Runtime::None => anyhow::bail!("{}", no_runtime_error()),
        Runtime::Userland(ulsh) => return ensure_userland(&ulsh, key).await,
        Runtime::Podman | Runtime::PodmanRemote => {}
    }
    touch(key);
    spawn_reaper_once();
    let lock = lock_for(key);
    let _g = lock.lock().await;
    touch(key);

    let root = envs_root();
    std::fs::create_dir_all(root.join(key))?;
    let workdir = workdir_for(key);
    let remote = podman_remote();
    if !remote {
        std::fs::create_dir_all(&workdir)?;
    }
    // The key is a HASH of the name, so a listing that only walks this
    // directory can show hashes and nothing else. Leave the name
    // beside the workspace so `.agent sandboxes list` can say which
    // box is which — an admin surface that answers in hashes is not an
    // admin surface.
    if let Some(name) = label_for(key) {
        let _ = std::fs::write(root.join(key).join("name"), name);
    }
    if !remote && !stays_under(&root, &workdir) {
        anyhow::bail!(
            "user env workspace {} escapes {} — refusing (symlink planted in user-envs?)",
            workdir.display(),
            root.display()
        );
    }

    let name = container_name(key);
    let cfg = EnvCfg::current();
    let bin = podman_bin();
    // Run this synchronously too: a recreated home may otherwise create
    // a new box while its previous generation is still running. No node
    // identity means no automatic reaping.
    reap_previous_incarnations(&bin).await;
    let Some(current) = current_owner() else {
        anyhow::bail!(
            "cannot establish this node's identity for user-env ownership; preserving all containers and volumes"
        );
    };
    let existing_owned = inspect_owned(&bin, "container", &name).await?;
    if let Some(resource) = &existing_owned {
        match &resource.owner {
            Some(owner) if owner.node != current.node || owner.home != current.home => {
                anyhow::bail!(
                    "user env {name} belongs to another node/home; refusing to remove or reuse it"
                );
            }
            Some(owner) if owner.generation != current.generation => {
                anyhow::bail!(
                    "user env {name} belongs to an older generation; refusing to reuse it before cleanup completes"
                );
            }
            // A pre-label container stays usable exactly as before. The
            // generation reaper never selects it; an explicit purge is
            // still the operator's destructive path.
            None => {}
            _ => {}
        }
        if let Some((true, volume_name)) = &resource.work_mount {
            if let Some(volume) = inspect_owned(&bin, "volume", volume_name).await? {
                match (&resource.owner, &volume.owner) {
                    (Some(container), Some(volume)) if container != volume => {
                        anyhow::bail!(
                            "user env {name} and its /work volume have different owners; refusing both"
                        );
                    }
                    (Some(container), None)
                        if container == &current && volume_name == &volume_for(key) =>
                    {
                        // This container already belongs to THIS exact
                        // node/home/generation, and its canonical legacy
                        // volume is already attached. Reusing that pair
                        // adopts nothing new. If a rebuild is required,
                        // the volume guard below still refuses it until
                        // an explicit keep-work purge migrates the data.
                    }
                    (Some(container), None) if container == &current => {
                        anyhow::bail!(
                            "user env {name} has a non-canonical unlabelled /work volume; refusing to adopt it implicitly"
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    // `Isolated` means "a namespace of its own, WITH outbound", and on a
    // rootless podman that outbound comes from pasta or slirp4netns. A
    // host with neither still creates the box — nothing errors — so the
    // only place this can be said is here, once, as a warning.
    //
    // Warn-only on purpose: an offline or helper-less host must not fence
    // everyone out of a working box, and "your sandbox has no internet"
    // is a thing the operator decides about, not a thing `ensure`
    // decides for them. `veles doctor --sandbox` reports the same finding
    // as a problem, which is the surface that is supposed to be run
    // after an install.
    if cfg.net == EnvNet::Isolated {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let net = rootless_net_state().await;
        if !net.isolated_has_outbound() && !WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!(
                "user env boxes are configured `isolated`, but this host is {}: they will \
                 have NO outbound and nothing can be published into them. Install pasta or \
                 slirp4netns, or set [sandbox] user_env_network = \"none\" to say so \
                 explicitly. Veles keeps running — this is a warning, not a refusal.",
                net.describe()
            );
        }
    }

    // The container must exist, be bound to THIS workdir and sit in the
    // network namespace the config asks for. One that is merely STOPPED
    // (the idle reaper, a podman restart) is started again, not rebuilt:
    // what a model installed into it — apt, pip — lives in the container
    // layer, and only /work is a volume, so a rebuild throws an
    // afternoon's setup away (2026-10-13: ImageMagick and 800 packages a
    // model had put into the IRC box; the module doc always said
    // "started when stopped"). Missing, mounted to a vanished/mismatched
    // workspace (a wiped home), or left over from another network
    // posture — those are removed and rebuilt. A newer `[sandbox] image`
    // is not a reason: a box keeps its image until `purge` (keeps /work).
    let work_str = work_source(key, &workdir);
    let existing = inspect_state(&name).await?;
    let mut rebuild = true;
    let mut start_only = false;
    if let Some(st) = &existing {
        match rebuild_reason(st, &work_str, cfg.net) {
            Some(why) => tracing::warn!(
                "user env {name}: {why} — rebuilding it (a box keeps what it was created \
                 with for as long as it lives)"
            ),
            None => {
                rebuild = false;
                start_only = !st.running;
            }
        }
    }
    if start_only {
        let mut start = tokio::process::Command::new(&bin);
        start.arg("start").arg(&name);
        match podman_output(&mut start).await {
            Ok(out) if out.status.success() => {
                tracing::info!("user env {name}: started again (kept, not rebuilt)");
            }
            Ok(out) => {
                tracing::warn!(
                    "user env {name}: podman start failed: {} — rebuilding it",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                rebuild = true;
            }
            Err(e) => {
                tracing::warn!("user env {name}: podman start failed: {e} — rebuilding it");
                rebuild = true;
            }
        }
    }
    if rebuild {
        // A damaged mount is NOT permission to rebuild somebody else's
        // box. The ownership gate above ran before the posture check;
        // an unowned legacy object is retained, not destroyed merely
        // because its mount or network needs attention.
        if let Some(resource) = &existing_owned {
            rebuild_allowed(resource, &current)
                .map_err(|why| anyhow::anyhow!("user env {name} needs rebuilding but {why}"))?;
        }
        // `rm -f` on a missing container just errors, ignored.
        let mut rm = tokio::process::Command::new(&bin);
        rm.arg("rm").arg("-f").arg(&name);
        let _ = podman_output(&mut rm).await;
        if remote {
            if let Some(resource) = inspect_owned(&bin, "volume", &work_str).await? {
                rebuild_allowed(&resource, &current).map_err(|why| {
                    anyhow::anyhow!("user env volume {work_str} needs rebuilding but {why}")
                })?;
            }
            let mut volume = tokio::process::Command::new(&bin);
            volume.arg("volume").arg("create").arg("--ignore");
            for label in current.labels() {
                volume.arg("--label").arg(label);
            }
            volume.arg(&work_str);
            let out = podman_output(&mut volume).await?;
            if !out.status.success() {
                anyhow::bail!(
                    "podman volume create {work_str} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        let mut create = tokio::process::Command::from(build_create_command_owned(
            &bin,
            &name,
            &work_str,
            &cfg,
            Some(&current),
        ));
        let out = podman_output(&mut create).await?;
        if !out.status.success() {
            anyhow::bail!(
                "podman create {} failed: {} (is the image \"{}\" built? scripts/build-sandbox-image.sh)",
                name,
                String::from_utf8_lossy(&out.stderr).trim(),
                cfg.image
            );
        }
        let mut start = tokio::process::Command::new(&bin);
        start.arg("start").arg(&name);
        let out = podman_output(&mut start).await?;
        if !out.status.success() {
            anyhow::bail!(
                "podman start {} failed: {}",
                name,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(workdir)
}

/// Run an argv inside the env. No shell, no quoting surface; the
/// caller passes already-split argv (framework-composed).
/// `timeout_secs == 0` means no client-side deadline.
pub async fn exec_argv(key: &str, argv: &[String], timeout_secs: u64) -> anyhow::Result<ExecOut> {
    // The slot is taken BEFORE `ensure`, so a burst against a cold box
    // cannot start twenty container creations either. A background
    // shell escapes the bound by design: `bg_start`'s exec returns as
    // soon as the runner is detached, so the slot is free while the
    // work runs on — bg tasks are accounted by task id, not here.
    let _slot = acquire_exec_slot(key).await?;
    let _workdir = ensure(key).await?;
    if let Runtime::Userland(ulsh) = runtime() {
        let out =
            userland_output(&ulsh, key, CONTAINER_WORK, argv, &[], None, timeout_secs).await?;
        return Ok(match out {
            Some(o) => ExecOut {
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                exit_code: o.status.code().unwrap_or(-1),
                timed_out: false,
            },
            None => ExecOut {
                timed_out: true,
                exit_code: -1,
                ..Default::default()
            },
        });
    }
    let name = container_name(key);
    let mut cmd = tokio::process::Command::from(build_exec_command(
        &podman_bin(),
        &name,
        CONTAINER_WORK,
        &[("HOME", CONTAINER_WORK.to_string())],
        argv,
    ));
    let fut = podman_output(&mut cmd);
    let out = if timeout_secs == 0 {
        Some(fut.await)
    } else {
        tokio::time::timeout(Duration::from_secs(timeout_secs), fut)
            .await
            .ok()
    };
    match out {
        Some(Ok(o)) => Ok(ExecOut {
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
            exit_code: o.status.code().unwrap_or(-1),
            timed_out: false,
        }),
        Some(Err(e)) => Err(e),
        None => Ok(ExecOut {
            timed_out: true,
            exit_code: -1,
            ..Default::default()
        }),
    }
}

/// Run a SHELL COMMAND inside the env, wrapped in the image's
/// `timeout` so a runaway dies inside the container (client-side
/// `timeout_secs + 10` is the backstop that still reports exit 124).
pub async fn exec_shell(key: &str, command: &str, timeout_secs: u64) -> anyhow::Result<ExecOut> {
    let argv = vec![
        "timeout".to_string(),
        "-k".to_string(),
        "5".to_string(),
        timeout_secs.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        command.to_string(),
    ];
    let mut out = exec_argv(key, &argv, timeout_secs + 10).await?;
    if out.exit_code == 124 || out.timed_out {
        out.timed_out = true;
    }
    Ok(out)
}

// ── file ops through the fence ────────────────────────────────────────

fn staging_dir() -> anyhow::Result<PathBuf> {
    let root = envs_root().join(".staging");
    std::fs::create_dir_all(&root)?;
    let d = root.join(format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Read a container file as bytes (resolved INSIDE the container
/// namespace — symlinks cannot hop the fence).
pub async fn read_bytes(key: &str, container_path: &str) -> anyhow::Result<Vec<u8>> {
    let _ = ensure(key).await?;
    if let Runtime::Userland(ulsh) = runtime() {
        let argv = vec![
            "cat".to_string(),
            "--".to_string(),
            container_path.to_string(),
        ];
        let out = userland_output(&ulsh, key, CONTAINER_WORK, &argv, &[], None, 120)
            .await?
            .ok_or_else(|| anyhow::anyhow!("reading {container_path} in the box took over 120s"))?;
        if !out.status.success() {
            anyhow::bail!(
                "reading {container_path} in the box failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return Ok(out.stdout);
    }
    let stage = staging_dir()?;
    let tmp = stage.join("out");
    let mut cmd = tokio::process::Command::from(build_cp_command(
        &podman_bin(),
        &format!("{}:{}", container_name(key), container_path),
        &tmp.display().to_string(),
    ));
    let out = podman_output(&mut cmd).await;
    let res = match out {
        Ok(o) if o.status.success() => std::fs::read(&tmp).map_err(|e| anyhow::anyhow!(e)),
        Ok(o) => anyhow::bail!(
            "podman cp {} failed: {}",
            container_path,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => anyhow::bail!("podman cp launch failed: {e}"),
    };
    let _ = std::fs::remove_dir_all(&stage);
    res
}

/// Read a session's own workspace file through the backend that owns it.
/// `/work/...`, `~/...` and relative paths belong to a sandboxed session;
/// every other path belongs to the host and is left to the caller's jail.
/// `None` means host, never a failed guest read disguised as a host miss.
pub async fn read_session_bytes(session: &str, path: &str) -> Option<anyhow::Result<Vec<u8>>> {
    // A Windows drive path is absolute even when it does not start '/':
    // it must never be resolved as a guest-relative `C:\...` file.
    if Path::new(path).is_absolute() && path != CONTAINER_WORK && !path.starts_with("/work/") {
        return None;
    }
    let Zone::Sandbox { key, name } = zone_for(Some(session), ZoneArg::Unset) else {
        return None;
    };
    let cpath = container_resolve(path);
    Some(
        read_bytes(&key, &cpath)
            .await
            .map_err(|e| anyhow::anyhow!("{cpath} in sandbox {name}: {e}")),
    )
}

/// Write bytes to a container path (parent dirs created inside).
pub async fn write_bytes(key: &str, container_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let _ = ensure(key).await?;
    if let Some(parent) = Path::new(container_path).parent() {
        let parent = parent.display().to_string();
        if !parent.is_empty() && parent != "/" {
            let argv = vec!["mkdir".to_string(), "-p".to_string(), parent.clone()];
            let out = exec_argv(key, &argv, 30).await?;
            if out.exit_code != 0 {
                anyhow::bail!("mkdir -p {parent} in env failed: {}", out.stderr);
            }
        }
    }
    if let Runtime::Userland(ulsh) = runtime() {
        return userland_write(&ulsh, key, container_path, bytes).await;
    }
    let stage = staging_dir()?;
    let tmp = stage.join("in");
    std::fs::write(&tmp, bytes)?;
    let mut cmd = tokio::process::Command::from(build_cp_command(
        &podman_bin(),
        &tmp.display().to_string(),
        &format!("{}:{}", container_name(key), container_path),
    ));
    let out = podman_output(&mut cmd).await;
    let _ = std::fs::remove_dir_all(&stage);
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => anyhow::bail!(
            "podman cp to {} failed: {}",
            container_path,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => anyhow::bail!("podman cp launch failed: {e}"),
    }
}

// ── sandbox-zone stale-write guard (content-hash ledger) ──────────────

fn sandbox_ledger() -> &'static Mutex<HashMap<String, u64>> {
    static L: std::sync::OnceLock<Mutex<HashMap<String, u64>>> = std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn content_hash(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// Record a sandbox read as the write baseline.
pub fn record_read(key: &str, container_path: &str, bytes: &[u8]) {
    sandbox_ledger()
        .lock()
        .unwrap()
        .insert(format!("{key}|{container_path}"), content_hash(bytes));
}

/// True when the file changed since the recorded read (same contract
/// as the host mtime ledger; hashes because container mtimes are not
/// cheaply readable from here and bytes are already in hand).
pub fn changed_since_read(key: &str, container_path: &str, bytes: &[u8]) -> bool {
    match sandbox_ledger()
        .lock()
        .unwrap()
        .get(&format!("{key}|{container_path}"))
    {
        Some(h) => *h != content_hash(bytes),
        None => false,
    }
}

// ── background shells inside an env ───────────────────────────────────

/// Launch a background command in the env. The runner lives in the
/// workspace (host-writable for staging, container-executable), writes
/// output/code files the waiter polls HOST-side through the
/// canonicalize guard, and reports its pid for cancel.
pub async fn bg_start(
    key: &str,
    task_id: u32,
    command: &str,
    workdir: &Path,
) -> anyhow::Result<u32> {
    let rel = format!(".veles-bg/{task_id}");
    let runner = format!(
        "#!/bin/sh\n# veles background shell runner (framework-generated)\n\
         cd {CONTAINER_WORK} || exit 97\nd={CONTAINER_WORK}/{rel}\n\
         mkdir -p \"$d\"\n( eval \"$(cat \"$d/cmd.txt\")\" ) > \"$d/out\" 2>&1\necho $? > \"$d/code\"\n"
    );
    stage_into(
        key,
        workdir,
        &rel,
        &[
            ("cmd.txt", command.as_bytes()),
            ("run.sh", runner.as_bytes()),
        ],
    )
    .await?;

    if let Runtime::Userland(ulsh) = runtime() {
        let argv = bg_launch_argv(task_id, true);
        let call = crate::userland::GuestCall {
            sandbox: Some(key),
            cwd: CONTAINER_WORK,
            home: Some(CONTAINER_WORK),
            env: &[],
            argv: &argv,
        };
        let mut cmd = tokio::process::Command::from(crate::userland::command(&ulsh, &call, |k| {
            std::env::var(k).ok()
        }));
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start ulsh ({}): {e}", ulsh.display()))?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("the background shell's ulsh exited at once"))?;
        userland_bg().lock_ok().insert(pid, child);
        return Ok(pid);
    }
    let argv = bg_launch_argv(task_id, false);
    let out = exec_argv(key, &argv, 30).await?;
    let pid: u32 = out
        .stdout
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("env runner did not report a pid: {}", out.stderr))?;
    Ok(pid)
}

/// The argv that starts a background shell's runner in its box.
///
/// podman (`held = false`): detached inside one `podman exec` (`setsid -w
/// sh run.sh & echo $!`) — the container keeps it alive, the exec returns
/// its pid, and the exec slot is free while it runs. The userland cannot
/// do that: proot runs with `--kill-on-exit`, so whatever a session
/// leaves behind dies with it, and the broker kills a session's whole
/// process group when its client hangs up. There (`held = true`) the
/// runner IS the session, `sh run.sh` in the foreground of a ulsh this
/// process keeps for the shell's whole life; its pid is that ulsh's, and
/// [`bg_state`]/[`bg_kill`] answer from it.
pub fn bg_launch_argv(task_id: u32, held: bool) -> Vec<String> {
    let run = format!("{CONTAINER_WORK}/.veles-bg/{task_id}/run.sh");
    if held {
        vec!["sh".to_string(), run]
    } else {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            // The async child is not a group leader, so util-linux
            // setsid puts the runner in a NEW session without forking
            // again. Its echoed pid is then also its PGID: cancelling
            // only run.sh used to leave detached grandchildren alive.
            format!("setsid -w sh {run} >/dev/null 2>&1 & echo $!"),
        ]
    }
}

/// Poll a background shell's files HOST-side, guard-first. Returns
/// `(appended_output, Option<exit_code>)`; a tampered output path is
/// reported, never read.
pub fn bg_poll(workdir: &Path, task_id: u32, since_byte: usize) -> (String, usize, Option<i32>) {
    let dir = workdir.join(".veles-bg").join(task_id.to_string());
    let mut out = String::new();
    let mut cursor = since_byte;
    let out_path = dir.join("out");
    if stays_under(workdir, &out_path) {
        if let Ok(b) = std::fs::read(&out_path) {
            if b.len() > since_byte {
                out = String::from_utf8_lossy(&b[since_byte..]).to_string();
                cursor = b.len();
            }
        }
    } else {
        out = "[sandbox] output path tampered — read dropped\n".into();
        // Never advance the cursor for tampered paths: refuse to make
        // progress on a file that left the workspace.
    }
    let code_path = dir.join("code");
    let code = if stays_under(workdir, &code_path) {
        std::fs::read_to_string(&code_path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
    } else {
        None
    };
    (out, cursor, code)
}

/// [`bg_poll`] for a remote podman, where there is no bind mount to read:
/// one `podman exec` per call. The exit code is read first and the
/// output's size second — the runner writes the code only after the
/// output is complete — and exactly the bytes up to that size are sent,
/// so the cursor is the container's byte count, whatever a lossy UTF-8
/// conversion does to the text.
pub async fn bg_poll_remote(
    key: &str,
    task_id: u32,
    since_byte: usize,
) -> (String, usize, Option<i32>) {
    let d = format!("{CONTAINER_WORK}/.veles-bg/{task_id}");
    let script = format!(
        "c=$(cat {d}/code 2>/dev/null); \
         n=$(wc -c < {d}/out 2>/dev/null | tr -d ' '); [ -n \"$n\" ] || n=0; \
         echo \"$c $n\"; \
         if [ \"$n\" -gt {since_byte} ]; then \
         tail -c +{} {d}/out | head -c $((n - {since_byte})); fi",
        since_byte + 1
    );
    let argv = vec!["sh".to_string(), "-c".to_string(), script];
    let Ok(out) = exec_argv(key, &argv, 30).await else {
        return (String::new(), since_byte, None);
    };
    let (head, chunk) = out
        .stdout
        .split_once('\n')
        .unwrap_or((out.stdout.as_str(), ""));
    let mut it = head.split_whitespace();
    let (first, second) = (it.next(), it.next());
    // An empty code makes the size the only field.
    let (code, size) = match second {
        Some(n) => (
            first.and_then(|c| c.parse::<i32>().ok()),
            n.parse::<usize>().ok(),
        ),
        None => (None, first.and_then(|n| n.parse::<usize>().ok())),
    };
    let cursor = size.filter(|n| *n > since_byte).unwrap_or(since_byte);
    (chunk.to_string(), cursor, code)
}

/// Liveness of a background pid inside the env ("alive", "dead", or
/// "done" when the code file exists) — one podman exec per probe.
pub async fn bg_state(key: &str, task_id: u32, pid: u32) -> anyhow::Result<&'static str> {
    if matches!(runtime(), Runtime::Userland(_)) {
        // Held by our own ulsh ([`bg_launch_argv`]): alive while it runs.
        // Once it has gone the runner has too; the caller's last poll
        // reads the exit code the runner left, if it left one.
        let mut held = userland_bg().lock_ok();
        return Ok(match held.get_mut(&pid).map(|c| c.try_wait()) {
            Some(Ok(None)) => "alive",
            Some(_) => {
                held.remove(&pid);
                "dead"
            }
            None => "dead",
        });
    }
    let script = format!(
        "d={CONTAINER_WORK}/.veles-bg/{task_id}; \
         if test -f \"$d/code\"; then echo done; \
         elif kill -0 {pid} 2>/dev/null; then echo alive; \
         else echo dead; fi"
    );
    let argv = vec!["sh".to_string(), "-c".to_string(), script];
    let out = exec_argv(key, &argv, 30).await?;
    Ok(match out.stdout.trim() {
        "done" => "done",
        "alive" => "alive",
        _ => "dead",
    })
}

/// Cancel a background pid inside the env (SIGTERM, best effort).
///
/// In the userland the pid is the ulsh holding the runner: SIGTERM to it
/// is forwarded to the guest's process group, and if the group has not
/// gone in 5 s ulsh is killed, whose hang-up makes the broker SIGKILL
/// the group.
pub async fn bg_kill(key: &str, pid: u32) -> anyhow::Result<()> {
    if matches!(runtime(), Runtime::Userland(_)) {
        if let Some(mut child) = userland_bg().lock_ok().remove(&pid) {
            crate::platform::request_stop(pid);
            crate::rt::spawn(async move {
                if tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            });
        }
        return Ok(());
    }
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        // The launcher's `setsid -w` makes this pid the runner's PGID.
        // Signal the group so detached descendants cannot outlive cancel;
        // escalation after a grace period reaches traps ignoring TERM.
        format!(
            "kill -TERM -{pid} 2>/dev/null || true; sleep 1; \
             kill -KILL -{pid} 2>/dev/null || true"
        ),
    ];
    exec_argv(key, &argv, 30).await?;
    Ok(())
}

// ── execute_code's safe interp inside the env ─────────────────────────

/// The driver's runner in a userland box: the box's own tclsh, found
/// rather than assumed (Alpine's `tcl` installs `tclsh` and `tclsh8.6`;
/// the minirootfs has neither until `apk add tcl`), under the guest's
/// `timeout`. `$1` the seconds, `$2` the driver.
pub const USERLAND_TCLSH: &str = "t=$(command -v tclsh || command -v tclsh8.6 || command -v \
     tclsh9.0) || { echo 'no tclsh in this box: apk add tcl (in the userland before the box \
     is first used, or in the box itself)' >&2; exit 127; }; exec timeout -k 5 \"$1\" \"$t\" \"$2\"";

/// Run the safe-interp driver (sandbox.rs) INSIDE the principal's env:
/// same Tcl fence, plus a persistent workspace — scripts can build
/// files that survive across calls. Staging uses a framework-owned,
/// guard-checked directory under the workspace.
pub async fn exec_tcl(
    key: &str,
    script: &str,
    timeout_secs: u64,
) -> anyhow::Result<crate::sandbox::SandboxOutput> {
    let workdir = ensure(key).await?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let rel = format!(".veles-run/{stamp}");
    let staged = stage_into(
        key,
        &workdir,
        &rel,
        &[
            ("user.tcl", script.as_bytes()),
            ("driver.tcl", crate::sandbox::SANDBOX_DRIVER.as_bytes()),
        ],
    )
    .await?;

    let in_container = format!("{CONTAINER_WORK}/{rel}");
    if let Runtime::Userland(ulsh) = runtime() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            USERLAND_TCLSH.to_string(),
            "veles-tcl".to_string(),
            timeout_secs.to_string(),
            format!("{in_container}/driver.tcl"),
        ];
        let env = [("VELES_SANDBOX_SCRIPT", format!("{in_container}/user.tcl"))];
        let res = userland_output(
            &ulsh,
            key,
            CONTAINER_WORK,
            &argv,
            &env,
            None,
            timeout_secs + 10,
        )
        .await;
        unstage(key, &rel, staged).await;
        return match res {
            Ok(Some(o)) => Ok(crate::sandbox::SandboxOutput {
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                exit_code: o.status.code().unwrap_or(-1),
                timed_out: o.status.code() == Some(124),
            }),
            Ok(None) => Ok(crate::sandbox::SandboxOutput {
                timed_out: true,
                exit_code: -1,
                ..Default::default()
            }),
            Err(e) => Err(e),
        };
    }
    let argv = vec![
        "timeout".to_string(),
        "-k".to_string(),
        "5".to_string(),
        timeout_secs.to_string(),
        "tclsh".to_string(),
        format!("{in_container}/driver.tcl"),
    ];
    let env = [
        ("HOME", CONTAINER_WORK.to_string()),
        ("TMPDIR", CONTAINER_WORK.to_string()),
        ("VELES_SANDBOX_SCRIPT", format!("{in_container}/user.tcl")),
    ];
    let name = container_name(key);
    let mut cmd = tokio::process::Command::from(build_exec_command(
        &podman_bin(),
        &name,
        CONTAINER_WORK,
        &env,
        &argv,
    ));
    let res = tokio::time::timeout(
        Duration::from_secs(timeout_secs + 10),
        podman_output(&mut cmd),
    )
    .await;
    unstage(key, &rel, staged).await;
    match res {
        Ok(Ok(o)) => Ok(crate::sandbox::SandboxOutput {
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
            exit_code: o.status.code().unwrap_or(-1),
            timed_out: o.status.code() == Some(124),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(crate::sandbox::SandboxOutput {
            timed_out: true,
            exit_code: -1,
            ..Default::default()
        }),
    }
}

// ── idle reaper + orphan GC ───────────────────────────────────────────

fn spawn_reaper_once() {
    static R: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        crate::rt::spawn(async move {
            loop {
                // Generation cleanup must not postpone the established
                // idle-stop pass behind a slow remote podman service.
                let bin = podman_bin();
                let previous = reap_previous_incarnations(&bin);
                let idle = reap_once();
                tokio::join!(previous, idle);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    });
}

async fn reap_once() {
    let idle_secs = crate::config::SandboxSection::current().user_env_idle_stop_secs;
    let bin = podman_bin();
    // 1. Our own keys, idle-stopped.
    if idle_secs > 0 {
        let idle: Vec<(String, std::time::Instant)> = {
            let l = last_use().lock().unwrap();
            l.iter()
                .filter(|(k, t)| t.elapsed().as_secs() > idle_secs && envs_root().join(k).exists())
                .map(|(k, t)| (k.clone(), *t))
                .collect()
        };
        for (key, _) in idle {
            let name = container_name(&key);
            if inspect_running(&name).await.unwrap_or(false) {
                let mut c = tokio::process::Command::new(&bin);
                c.arg("stop").arg("-t").arg("10").arg(&name);
                let _ = podman_output(&mut c).await;
                tracing::info!("user env {key}: idle-stopped after {idle_secs}s");
            }
        }
    }
    // 2. Orphan GC: env containers (any process's) whose /work source
    //    has vanished. A wiped scratch home must not leave zombies.
    //    Concurrent veles processes with LIVE workspaces are left
    //    alone — only provably-dead mounts are removed.
    let mut ps = tokio::process::Command::new(&bin);
    ps.args([
        "ps",
        "-a",
        "--filter",
        &format!("name={NAME_PREFIX}"),
        "--format",
        "{{.Names}}",
    ]);
    if let Ok(out) = podman_output(&mut ps).await {
        for name in String::from_utf8_lossy(&out.stdout).lines() {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            // A volume is never "gone" from here: its data lives on the
            // podman service's side, and under a remote podman a path
            // check from this machine would call every box an orphan.
            if let Some((false, src)) = inspect_work_source(name).await {
                if !Path::new(&src).exists() {
                    let mut rm = tokio::process::Command::new(&bin);
                    rm.arg("rm").arg("-f").arg(name);
                    let _ = podman_output(&mut rm).await;
                    tracing::info!("user env orphan removed: {name} (workspace {src} gone)");
                }
            }
        }
    }
}

// ── admin surface ─────────────────────────────────────────────────────

/// One env row for the `.agent sandboxes` listing.
/// The human name a key was created for, recorded beside its
/// workspace. `None` for an env created before names existed.
pub fn stored_name(key: &str) -> Option<String> {
    let raw = std::fs::read_to_string(envs_root().join(key).join("name")).ok()?;
    let t = raw.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Remember which name minted this key, so `ensure` can write it down.
/// Set by `zone_for` — the only place that knows both.
fn remember_label(key: &str, name: &str) {
    labels().lock_ok().insert(key.to_string(), name.to_string());
}

fn label_for(key: &str) -> Option<String> {
    labels().lock_ok().get(key).cloned()
}

fn labels() -> &'static Mutex<HashMap<String, String>> {
    static L: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn status_one(key: &str) -> Value {
    if matches!(runtime(), Runtime::Userland(_)) {
        return serde_json::json!({
            "key": key,
            "name": stored_name(key).or_else(|| label_for(key)),
            "container": format!("userland sandbox {key}"),
            "running": userland_opened().lock_ok().contains(key),
            "workspace": format!(
                "{CONTAINER_WORK} in Veles userland's files/sandboxes/{key}"
            ),
        });
    }
    let name = container_name(key);
    let running = inspect_running(&name).await.unwrap_or(false);
    serde_json::json!({
        "key": key,
        "name": stored_name(key).or_else(|| label_for(key)),
        "container": name,
        "running": running,
        "workspace": if podman_remote() {
            format!("volume {}", volume_for(key))
        } else {
            workdir_for(key).display().to_string()
        },
    })
}

/// Every env known on disk under the current home.
pub async fn status_list() -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(envs_root()) {
        let mut keys: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with('.'))
            .collect();
        keys.sort();
        for k in keys {
            out.push(status_one(&k).await);
        }
    }
    out
}

/// `veles doctor --sandbox`: one box end to end — created, a command run
/// in it, a file in and back out through `podman cp`, the safe-interp
/// driver run in its workspace, and the box removed with its workspace.
/// User envs are podman whatever `[sandbox] backend` says (but
/// `userland`, whose boxes this exercises the same way), so this is the
/// half of the fence the backend's own probe never touches.
pub async fn doctor_round_trip() -> anyhow::Result<()> {
    // A userland box outlives its purge (only its /work is emptied: the
    // guest root is the userland's to remove), so the doctor reuses one
    // box there instead of leaving a rootfs copy per run.
    let key = if matches!(runtime(), Runtime::Userland(_)) {
        key_for("doctor")
    } else {
        key_for(&format!("doctor-{}", std::process::id()))
    };
    let res = async {
        ensure(&key).await?;
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo ok-$((6*7))".to_string(),
        ];
        let out = exec_argv(&key, &argv, 60).await?;
        anyhow::ensure!(
            out.stdout.trim() == "ok-42",
            "exec said {:?} (exit {}): {}",
            out.stdout.trim(),
            out.exit_code,
            out.stderr.trim()
        );
        write_bytes(&key, "/work/doctor.txt", b"through the fence").await?;
        let back = read_bytes(&key, "/work/doctor.txt").await?;
        anyhow::ensure!(
            back == b"through the fence",
            "podman cp round trip changed the file"
        );
        let tcl = exec_tcl(&key, "puts [expr {6*7}]", 60).await?;
        anyhow::ensure!(
            tcl.stdout.trim() == "42",
            "the driver said {:?} (exit {}): {}",
            tcl.stdout.trim(),
            tcl.exit_code,
            tcl.stderr.trim()
        );
        Ok(())
    }
    .await;
    let _ = purge(&key, true).await;
    res
}

async fn create_volume(bin: &str, name: &str, owner: Option<&EnvOwner>) -> anyhow::Result<()> {
    let mut create = tokio::process::Command::new(bin);
    create.arg("volume").arg("create");
    if let Some(owner) = owner {
        for label in owner.labels() {
            create.arg("--label").arg(label);
        }
    }
    create.arg(name);
    let out = podman_output(&mut create).await?;
    if !out.status.success() {
        anyhow::bail!(
            "podman volume create {name} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn remove_volume(bin: &str, name: &str) -> anyhow::Result<()> {
    let mut rm = tokio::process::Command::new(bin);
    rm.arg("volume").arg("rm").arg("-f").arg(name);
    let out = podman_output(&mut rm).await?;
    if !out.status.success() {
        anyhow::bail!(
            "podman volume rm {name} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Podman 5 cannot change labels on an existing volume, and
/// `volume create --ignore --label ...` silently leaves them alone.
/// Keep the exported work in a caller-side file: under a remote podman
/// the archive is streamed over its connection, not a host bind path.
struct RetainedLegacyWork {
    volume: String,
    staged: String,
    archive: Option<tempfile::TempDir>,
    committed: bool,
}

impl RetainedLegacyWork {
    fn archive(&self) -> PathBuf {
        self.archive
            .as_ref()
            .expect("migration archive until guard is disarmed")
            .path()
            .join("work.tar")
    }

    fn disarm(&mut self) {
        self.committed = true;
    }

    async fn discard(mut self, bin: &str) {
        // Only the random staging volume made by this operation. The
        // caller's original volume is never removed by a failed prepare.
        if let Err(error) = remove_volume(bin, &self.staged).await {
            tracing::warn!("user env migration staging volume retained: {error:#}");
        }
        self.disarm();
    }

    async fn prepare(
        bin: &str,
        container: &OwnedResource,
        expected_volume: &str,
        current: &EnvOwner,
    ) -> anyhow::Result<Option<Self>> {
        if container.ownership_incomplete {
            anyhow::bail!(
                "user env {} has incomplete ownership labels; its volume is preserved",
                container.id
            );
        }
        match &container.owner {
            Some(owner) if owner != current => {
                anyhow::bail!("refusing to migrate a volume from a foreign or older container")
            }
            Some(_) | None => {}
        }
        let Some((true, volume)) = &container.work_mount else {
            return Ok(None);
        };
        if volume != expected_volume {
            // A custom legacy mount is not proof that it belongs to this
            // key's canonical volume. Preserve it, but do not adopt it.
            return Ok(None);
        }
        let Some(resource) = inspect_owned(bin, "volume", volume).await? else {
            return Ok(None);
        };
        if resource.ownership_incomplete {
            anyhow::bail!(
                "legacy volume {volume} has incomplete ownership labels; it is preserved"
            );
        }
        if resource.owner.is_some() {
            return Ok(None);
        }
        let archive = tempfile::Builder::new()
            .prefix("veles-userenv-migrate-")
            .tempdir()?;
        let archive_path = archive.path().join("work.tar");
        let mut export = tokio::process::Command::new(bin);
        export
            .arg("volume")
            .arg("export")
            .arg("--output")
            .arg(&archive_path)
            .arg(&resource.id);
        let out = podman_output(&mut export).await?;
        if !out.status.success() {
            anyhow::bail!(
                "cannot export legacy work volume {volume}; it is preserved: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // Deliberately outside VOLUME_PREFIX: if this operation is
        // cancelled, its recovery copy must never be selected by the
        // managed old-generation reaper. The Drop guard names it.
        let staged = format!("veles-userenv-recovery-{}", uuid::Uuid::new_v4().simple());
        let result = async {
            create_volume(bin, &staged, Some(current)).await?;
            import_volume(bin, &staged, &archive_path).await?;
            verify_volume_owner(bin, &staged, current).await
        }
        .await;
        if let Err(error) = result {
            if let Err(cleanup) = remove_volume(bin, &staged).await {
                tracing::warn!("user env migration staging volume retained: {cleanup:#}");
            }
            return Err(error.context(format!(
                "legacy work volume {volume} was NOT removed; its labelled migration failed"
            )));
        }
        Ok(Some(Self {
            volume: volume.clone(),
            staged,
            archive: Some(archive),
            committed: false,
        }))
    }

    /// The staged, verified archive is complete before the original is
    /// removed. If installing the canonical replacement fails, restore
    /// the original from the same archive rather than losing its work.
    async fn install_for(mut self, bin: &str, current: &EnvOwner) -> anyhow::Result<()> {
        let archive = self.archive();
        if let Err(error) = remove_volume(bin, &self.volume).await {
            self.discard(bin).await;
            return Err(error.context("legacy work volume was not removed"));
        }
        let result = async {
            create_volume(bin, &self.volume, Some(current)).await?;
            import_volume(bin, &self.volume, &archive).await?;
            verify_volume_owner(bin, &self.volume, current).await
        }
        .await;
        if result.is_ok() {
            if let Err(error) = remove_volume(bin, &self.staged).await {
                // The canonical, verified replacement is already live;
                // do not report its migration as failed because the
                // temporary duplicate needs a later cleanup.
                tracing::warn!("user env migration staging volume retained: {error:#}");
            }
            self.disarm();
            return Ok(());
        }
        let replacement_error = result.unwrap_err();
        let restored = async {
            // The canonical name, if it exists now, is our partial
            // replacement: the old one was removed above and the caller
            // still holds this key's lifecycle lock.
            remove_volume(bin, &self.volume).await?;
            create_volume(bin, &self.volume, None).await?;
            import_volume(bin, &self.volume, &archive).await?;
            if inspect_owned(bin, "volume", &self.volume).await?.is_none() {
                anyhow::bail!("restored volume is absent");
            }
            Ok(())
        }
        .await;
        match restored {
            Ok(()) => {
                self.discard(bin).await;
                Err(replacement_error.context(
                    "labelled replacement failed; the original legacy volume was restored from its export",
                ))
            }
            Err(restore_error) => {
                let archive = self
                    .archive
                    .take()
                    .expect("recovery archive is present")
                    .keep();
                self.disarm();
                Err(anyhow::anyhow!(
                    "labelled replacement failed: {replacement_error:#}; restoring the original also failed: {restore_error:#}. Recovery archive retained at {}; staged verified copy is volume {}",
                    archive.display(),
                    self.staged,
                ))
            }
        }
    }
}

impl Drop for RetainedLegacyWork {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // A cancelled future cannot await remote cleanup. Keep the
        // verified staging copy and the archive rather than silently
        // deleting either, and name them for an operator's recovery.
        // No blocking podman call belongs in Drop.
        let archive = self
            .archive
            .take()
            .expect("recovery archive is present")
            .keep();
        tracing::warn!(
            "user env legacy migration interrupted; original volume {} may need recovery, verified staging volume is {}, archive is {}",
            self.volume,
            self.staged,
            archive.display(),
        );
    }
}

async fn import_volume(bin: &str, name: &str, archive: &Path) -> anyhow::Result<()> {
    let mut import = tokio::process::Command::new(bin);
    import.arg("volume").arg("import").arg(name).arg(archive);
    let out = podman_output(&mut import).await?;
    if !out.status.success() {
        anyhow::bail!(
            "podman volume import {name} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn verify_volume_owner(bin: &str, name: &str, current: &EnvOwner) -> anyhow::Result<()> {
    let resource = inspect_owned(bin, "volume", name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("volume {name} disappeared during migration"))?;
    if resource.owner.as_ref() != Some(current) {
        anyhow::bail!("volume {name} does not carry this node/home/generation after migration");
    }
    Ok(())
}

/// Remove an env container; `delete_work` also removes the persistent
/// workspace. The container is recreated on the principal's next
/// sandboxed call (with a fresh empty workspace if deleted).
pub async fn purge(key: &str, delete_work: bool) -> anyhow::Result<()> {
    if matches!(runtime(), Runtime::Userland(_)) {
        // There is no container to remove: the box is a guest root in the
        // userland app's storage, which this app cannot delete. Emptying
        // /work is what a purge can do; the root stays (an open item:
        // docs/issues/open-items/phone-userland.md).
        if delete_work {
            let argv = vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("rm -rf {CONTAINER_WORK} && mkdir -p {CONTAINER_WORK}"),
            ];
            let out = exec_argv(key, &argv, 120).await?;
            if out.exit_code != 0 {
                anyhow::bail!("emptying the box's /work failed: {}", out.stderr.trim());
            }
            let dir = envs_root().join(key);
            if dir.exists() && stays_under(&envs_root(), &dir) {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
        userland_opened().lock_ok().remove(key);
        last_use().lock().unwrap().remove(key);
        return Ok(());
    }
    let bin = podman_bin();
    let remote = podman_remote();
    let lock = lock_for(key);
    let _g = lock.lock().await;
    let name = container_name(key);
    // Inspect and detach under the same per-key lock as ensure. This is
    // especially important for the legacy-volume migration below: a new
    // ensure must not mount a volume while its archive is being taken.
    let container = inspect_owned(&bin, "container", &name).await?;
    if !delete_work && remote {
        if let Some(container) = &container {
            let Some(current) = current_owner() else {
                anyhow::bail!(
                    "cannot establish node identity for keep-work ownership migration; preserving the container"
                );
            };
            // Stage BEFORE removing the container. A failed export or
            // import therefore leaves both the live container and its
            // work available; the original is removed only after the
            // staged labelled replacement has been verified.
            if let Some(retained) =
                RetainedLegacyWork::prepare(&bin, container, &volume_for(key), &current).await?
            {
                retained.install_for(&bin, &current).await?;
                tracing::info!(
                    "user env {name}: retained legacy work volume migrated with ownership labels"
                );
            }
        }
    }
    let mut c = tokio::process::Command::new(&bin);
    c.arg("rm").arg("-f").arg(&name);
    let out = podman_output(&mut c).await?;
    if !out.status.success() {
        anyhow::bail!(
            "podman rm failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if delete_work && remote {
        let mut v = tokio::process::Command::new(&bin);
        v.arg("volume").arg("rm").arg("-f").arg(volume_for(key));
        let _ = podman_output(&mut v).await;
    }
    if delete_work {
        let dir = envs_root().join(key);
        if dir.exists() && !stays_under(&envs_root(), &dir) {
            anyhow::bail!(
                "refusing to delete a workspace that escapes {}",
                envs_root().display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    last_use().lock().unwrap().remove(key);
    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(c: &std::process::Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn previous_incarnation_selection_covers_the_shared_socket_case() {
        let owner = |node: &str, home: &str, generation: &str| EnvOwner {
            node: node.to_string(),
            home: PathBuf::from(home),
            generation: generation.to_string(),
        };
        let current = owner("production", "/opt/data", "new");
        assert!(is_previous_incarnation(
            &owner("production", "/opt/data", "old"),
            &current
        ));
        for preserved in [
            owner("scratch", "/opt/data", "old"),
            owner("production", "/different", "old"),
            current.clone(),
        ] {
            assert!(!is_previous_incarnation(&preserved, &current));
        }
    }

    /// A node with no podman cannot make a box, and says what fixes it;
    /// a remote podman (CONTAINER_HOST) is a runtime.
    #[test]
    fn no_podman_is_no_sandbox_and_names_the_fix() {
        let _g = crate::testenv::guard();
        let (host, bin) = (
            std::env::var("CONTAINER_HOST").ok(),
            std::env::var("VELES_PODMAN_BIN").ok(),
        );
        std::env::remove_var("CONTAINER_HOST");
        std::env::set_var("VELES_PODMAN_BIN", "/nonexistent/podman");
        let without = runtime_available();
        let why = no_runtime_error();
        std::env::set_var("CONTAINER_HOST", "unix:///nonexistent/podman.sock");
        let remote = runtime_available();
        for (k, v) in [("CONTAINER_HOST", host), ("VELES_PODMAN_BIN", bin)] {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        assert!(!without, "no podman binary, no socket: no runtime");
        assert!(why.contains("sandbox_home"), "{why}");
        assert!(why.contains("/nonexistent/podman"), "{why}");
        assert!(remote, "a remote podman is a runtime");
    }

    /// Under a remote podman /work is a named volume: the argv names the
    /// volume, and no path from this machine reaches the service.
    #[test]
    fn a_remote_box_mounts_its_volume_not_a_host_path() {
        let cfg = EnvCfg {
            image: "veles-sandbox:latest".into(),
            memory_mb: 256,
            net: EnvNet::Isolated,
            drop_caps: true,
        };
        let a = args_of(&build_create_command_src(
            "podman",
            "veles-user-env-k",
            &volume_for("k"),
            &cfg,
        ));
        let joined = a.join(" ");
        assert!(joined.contains("-v veles-env-k:/work"), "{joined}");
        assert!(!joined.contains("--network"), "isolated: {joined}");
        assert!(joined.contains("--cap-drop ALL"), "{joined}");
        assert!(joined.contains("no-new-privileges"), "{joined}");
    }

    /// `podman inspect`'s mounts: a bind is matched by its host path, a
    /// volume by its NAME — its `Source` is a path on the service's host,
    /// which the orphan GC must never test from here.
    #[test]
    fn the_work_mount_is_read_by_kind() {
        let bind = serde_json::json!([
            {"Type": "bind", "Source": "/h/user-envs/k/work", "Destination": "/work"}
        ]);
        assert_eq!(
            work_mount_of(&bind),
            Some((false, "/h/user-envs/k/work".to_string()))
        );
        let vol = serde_json::json!([
            {"Type": "volume", "Name": "veles-env-k",
             "Source": "/srv/pool/veles-sbx/.local/share/containers/storage/volumes/veles-env-k/_data",
             "Destination": "/work"},
            {"Type": "bind", "Source": "/etc/hosts", "Destination": "/etc/hosts"}
        ]);
        assert_eq!(work_mount_of(&vol), Some((true, "veles-env-k".to_string())));
        assert_eq!(work_mount_of(&serde_json::json!([])), None);
    }

    #[test]
    fn owned_inspect_reads_mounts_and_labels_from_the_whole_object() {
        let object = serde_json::json!({
            "Id": "container-id",
            "Config": {"Labels": {
                NODE_LABEL: "this-node",
                HOME_LABEL: "/opt/data",
                INCARNATION_LABEL: "current"
            }},
            "Mounts": [{"Type": "volume", "Name": "veles-env-k",
                        "Destination": "/work"}]
        });
        let resource = owned_resource_from_inspect(&object).expect("container id");
        assert_eq!(resource.work_mount, Some((true, "veles-env-k".into())));
        assert_eq!(resource.owner.expect("labels").generation, "current");
        let volume = serde_json::json!({
            "Name": "veles-env-k",
            "Labels": {NODE_LABEL: "this-node", HOME_LABEL: "/opt/data",
                       INCARNATION_LABEL: "current"}
        });
        assert_eq!(
            owned_resource_from_inspect(&volume)
                .expect("volume name")
                .owner
                .expect("top-level volume labels")
                .generation,
            "current"
        );
    }

    #[test]
    fn create_command_pins_the_fence() {
        let cfg = EnvCfg {
            image: "veles-sandbox:latest".into(),
            memory_mb: 256,
            net: EnvNet::None,
            drop_caps: true,
        };
        let c = build_create_command("podman", "veles-user-env-x", Path::new("/h/w"), &cfg);
        assert_eq!(c.get_program().to_string_lossy(), "podman");
        let a = args_of(&c);
        for pin in [
            "create",
            "--pull=never",
            "--name",
            "veles-user-env-x",
            "--network",
            "none",
            "--memory",
            "256m",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--hostname",
            "veles-user-env",
            "-v",
            "/h/w:/work",
            "-w",
            "/work",
            "sleep",
            "infinity",
        ] {
            assert!(a.contains(&pin.to_string()), "missing {pin} in {a:?}");
        }
        // no-new-privileges and cap-drop hold in every posture — only
        // the network moves.
        //
        // CHURN, 2026-09-09: this half used to assert `--network host`
        // for the dev posture, i.e. it pinned the escalation as correct.
        // The dev posture is now ISOLATED, which passes no flag at all,
        // so the assertion is that the flag is ABSENT — and `host`
        // still reachable, but only by asking for it by name.
        let cfg_dev = EnvCfg {
            net: EnvNet::Isolated,
            ..cfg.clone()
        };
        let a2 = args_of(&build_create_command(
            "podman",
            "veles-user-env-x",
            Path::new("/h/w"),
            &cfg_dev,
        ));
        assert!(
            !a2.iter().any(|x| x == "--network"),
            "isolated passes NO network flag, so podman uses pasta: {a2:?}"
        );
        assert!(a2.contains(&"--cap-drop".to_string()));

        let cfg_host = EnvCfg {
            net: EnvNet::Host,
            ..cfg
        };
        let a3 = args_of(&build_create_command(
            "podman",
            "veles-user-env-x",
            Path::new("/h/w"),
            &cfg_host,
        ));
        let net = a3.iter().position(|x| x == "--network").unwrap();
        assert_eq!(a3[net + 1], "host", "host stays reachable when NAMED");
    }

    /// `network = false` is the untrusted posture and it means no
    /// network — the three-way knob must not be able to widen it back.
    #[test]
    fn network_false_outranks_the_posture_knob() {
        let mut s = crate::config::SandboxSection {
            network: false,
            user_env_network: "host".into(),
            ..Default::default()
        };
        assert_eq!(EnvCfg::from_section(&s).net, EnvNet::None);
        s.network = true;
        assert_eq!(EnvCfg::from_section(&s).net, EnvNet::Host);
    }

    /// The staleness rule that makes an upgrade reach a RUNNING
    /// deployment: a box built under the old `host` posture must read
    /// as stale under the new one, or the fix only protects people who
    /// had not used the bot yet.
    #[test]
    fn a_box_from_an_older_posture_reads_as_stale() {
        assert!(!net_mode_matches("host", EnvNet::Isolated));
        assert!(!net_mode_matches("host", EnvNet::None));
        assert!(net_mode_matches("host", EnvNet::Host));
        assert!(net_mode_matches("none", EnvNet::None));
        assert!(!net_mode_matches("none", EnvNet::Isolated));
        // Isolated is "not one of the named modes", so the rootless
        // default matches under whatever name this podman gives it —
        // pinning "pasta" would make a podman upgrade a rebuild loop.
        for spelling in ["pasta", "slirp4netns", "bridge", "private"] {
            assert!(
                net_mode_matches(spelling, EnvNet::Isolated),
                "{spelling} should satisfy Isolated"
            );
        }
    }

    /// A stopped box is started, not rebuilt. What a model installed into
    /// it (apt, pip) is in the container layer and only /work is a
    /// volume, so the old "not running → rebuild" threw that away at
    /// every idle-stop (2026-10-13). A moved /work or a posture mismatch
    /// still rebuilds, running or not.
    #[test]
    fn a_stopped_box_that_still_fits_is_started_not_rebuilt() {
        let st = |running: bool, work: &str, net: &str| ContainerState {
            running,
            work_source: Some(work.to_string()),
            net_mode: net.to_string(),
        };
        let k = "veles-env-k";
        assert_eq!(
            rebuild_reason(&st(false, k, "pasta"), k, EnvNet::Isolated),
            None
        );
        assert_eq!(
            rebuild_reason(&st(true, k, "pasta"), k, EnvNet::Isolated),
            None
        );
        assert_eq!(rebuild_reason(&st(false, k, "host"), k, EnvNet::Host), None);
        assert!(
            rebuild_reason(&st(false, "/old/home/work", "pasta"), k, EnvNet::Isolated).is_some()
        );
        assert!(rebuild_reason(&st(true, k, "host"), k, EnvNet::Isolated).is_some());
        assert!(rebuild_reason(&st(false, k, "host"), k, EnvNet::Isolated).is_some());
        let unbound = ContainerState {
            running: false,
            work_source: None,
            net_mode: "pasta".to_string(),
        };
        assert!(rebuild_reason(&unbound, k, EnvNet::Isolated).is_some());
    }

    /// The host question `Isolated` cannot answer for itself, pinned
    /// against captured `podman info` JSON rather than against the
    /// machine running the test.
    ///
    /// On a NixOS host that machine always has pasta (podman ships its
    /// own copy in `libexec/podman/`, so `command -v pasta` says MISSING
    /// while podman finds it) and the interesting branch could never be
    /// reached. Two details of the real payload are the point of the
    /// test: a helper that is not installed is reported as an EMPTY
    /// `executable` string, not an absent key, and `rootless` lives
    /// under `security`, not at the top of `.Host`.
    #[test]
    fn rootless_without_a_network_driver_is_the_case_being_detected() {
        // Captured on this host: rootless, pasta present, slirp4netns not.
        let with_pasta = serde_json::json!({
            "networkBackend": "netavark",
            "rootlessNetworkCmd": "pasta",
            "security": { "apparmorEnabled": false, "rootless": true },
            "slirp4netns": { "executable": "" },
            "pasta": {
                "executable": "/nix/store/dffjc7kh3r0639ag1g18gs2scpdk9z97-podman-5.8.6/libexec/podman/pasta",
                "version": "pasta 2026_07_16.090d739"
            }
        });
        let st = rootless_net_from_info(&with_pasta);
        assert!(st.probed && st.rootless && st.driver_present, "{st:?}");
        assert_eq!(st.backend, "pasta");
        assert!(
            st.isolated_has_outbound(),
            "a host with the driver podman intends to use is the healthy case"
        );

        // The failure this exists for: rootless, and NEITHER helper
        // installed. Note the empty strings — an `is_null()` check on the
        // key would read this host as fine.
        let no_driver = serde_json::json!({
            "rootlessNetworkCmd": "slirp4netns",
            "security": { "rootless": true },
            "slirp4netns": { "executable": "" },
            "pasta": { "executable": "" }
        });
        let st = rootless_net_from_info(&no_driver);
        assert!(st.probed && st.rootless, "{st:?}");
        assert!(
            !st.driver_present && !st.isolated_has_outbound(),
            "this is exactly the host where `isolated` silently means `none`: {st:?}"
        );
        let said = st.describe();
        assert!(
            said.contains("slirp4netns") && said.contains("not installed"),
            "the finding has to name the helper and say what is missing: {said}"
        );

        // The other helper satisfies it just as well — older installs
        // have slirp4netns and no pasta, and the fence must not require
        // today's spelling.
        let with_slirp = serde_json::json!({
            "rootlessNetworkCmd": "slirp4netns",
            "security": { "rootless": true },
            "slirp4netns": { "executable": "/usr/bin/slirp4netns" },
            "pasta": { "executable": "" }
        });
        assert!(rootless_net_from_info(&with_slirp).isolated_has_outbound());

        // A rooted podman builds a real bridge: the driver question does
        // not arise, and a host that is missing both helpers must not be
        // warned about a fence that does not depend on them.
        let rooted = serde_json::json!({
            "rootlessNetworkCmd": "pasta",
            "security": { "rootless": false },
            "slirp4netns": { "executable": "" },
            "pasta": { "executable": "" }
        });
        let st = rootless_net_from_info(&rooted);
        assert!(st.probed && !st.rootless, "{st:?}");
        assert!(
            st.isolated_has_outbound(),
            "rooted podman does not need a rootless driver: {st:?}"
        );
    }

    /// A podman that cannot be asked is not a broken fence.
    ///
    /// `probe_rootless_net` returns the default when the binary is
    /// missing, the daemon is down, or the JSON does not parse, and the
    /// default has to answer "outbound, as far as we know": the
    /// alternative is a warning about networking on every host where
    /// `podman info` is slow or absent, which trains the operator to
    /// ignore the line that matters.
    #[test]
    fn an_unprobeable_host_says_nothing_about_outbound() {
        let st = RootlessNet::default();
        assert!(!st.probed);
        assert!(
            st.isolated_has_outbound(),
            "unknown must not read as broken: {st:?}"
        );
        assert!(
            st.describe().contains("did not answer"),
            "and must say it was not asked, not that all is well: {}",
            st.describe()
        );

        // An older podman that reports neither `security` nor the helper
        // objects lands in the same place by the same rule: absent
        // `rootless` reads as "not rootless", which is the
        // say-nothing branch. podman has reported `security.rootless`
        // since 1.6, so this is a floor, not a live case.
        let sparse = serde_json::json!({ "networkBackend": "netavark" });
        let st = rootless_net_from_info(&sparse);
        assert!(st.probed && !st.rootless && !st.driver_present, "{st:?}");
        assert!(st.isolated_has_outbound(), "{st:?}");
    }

    /// "Fenced" is the predicate the approval gate leans on, so every
    /// way of opening the box up has to move it. A gate that skipped
    /// because it believed in a fence that had been unlocked would be
    /// worse than no skip at all.
    #[test]
    fn a_box_is_fenced_only_while_every_part_of_the_fence_holds() {
        let fenced = |s: &crate::config::SandboxSection| {
            let cfg = EnvCfg::from_section(s);
            cfg.net != EnvNet::Host
                && cfg.drop_caps
                && s.mounts.is_empty()
                && s.extra_args.is_empty()
        };
        let base = crate::config::SandboxSection::default();
        assert!(fenced(&base), "the shipped default is a fence");

        // The host's network namespace is the loopback escalation.
        let mut s = base.clone();
        s.user_env_network = "host".into();
        assert!(!fenced(&s), "--network host is not a fence");

        // Capabilities back on.
        let mut s = base.clone();
        s.capabilities = "default".into();
        assert!(!fenced(&s), "kept capabilities are not a fence");

        // A host path mounted in. Not applied to user envs TODAY, which
        // is exactly why the predicate reads it: the day it is, this
        // must already know.
        let mut s = base.clone();
        s.mounts = vec![crate::config::SandboxMount {
            host: "/etc".into(),
            container: "/etc".into(),
            mode: "ro".into(),
        }];
        assert!(!fenced(&s), "a host mount is not a fence");

        // The backend escape hatch can undo any of it.
        let mut s = base.clone();
        s.extra_args = vec!["--privileged".into()];
        assert!(!fenced(&s), "extra_args is not a fence");

        // `network = false` is TIGHTER, not looser.
        let mut s = base.clone();
        s.network = false;
        assert!(fenced(&s), "no network at all is still a fence");
    }

    /// A typo must not silently hand out the host's loopback.
    #[test]
    fn an_unknown_posture_falls_back_to_the_tighter_answer() {
        assert_eq!(EnvNet::parse("wat"), EnvNet::Isolated);
        assert_eq!(EnvNet::parse(""), EnvNet::Isolated);
        assert_eq!(EnvNet::parse("HOST"), EnvNet::Host);
        assert_eq!(EnvNet::parse("none"), EnvNet::None);
    }

    /// A user env must NOT share the host's network namespace.
    ///
    /// `--network host` put every below-Admin principal's "sandbox" on
    /// the host's loopback, where `desktop_api` serves the operator API
    /// with no auth on the stated reasoning that "nobody else can reach
    /// loopback". One `curl` from inside a channel member's box was
    /// therefore a full escalation to owner — and it defeated the
    /// claim (`acl.rs`) that sandbox-scoped caps never buy host access.
    ///
    /// This pins the DEFAULT section, because the default is what
    /// ships: reading the knob correctly does not help if the value it
    /// reads is `host`.
    #[test]
    fn the_default_posture_never_shares_the_hosts_netns() {
        let cfg = EnvCfg::from_section(&crate::config::SandboxSection::default());
        let a = args_of(&build_create_command(
            "podman",
            "veles-user-env-x",
            Path::new("/h/w"),
            &cfg,
        ));
        assert!(
            !a.contains(&"host".to_string()),
            "the default env posture put the box on the HOST netns: {a:?}"
        );
    }

    #[test]
    fn exec_command_carries_no_caller_env() {
        let c = build_exec_command(
            "podman",
            "n",
            "/work",
            &[("HOME", "/work".into())],
            &["rg".to_string(), "-n".to_string(), "x".to_string()],
        );
        let a = args_of(&c);
        // Exactly ONE -e pair — the framework's HOME. If the gateway
        // env ever crossed here, F3 would be a memory.
        assert_eq!(a.iter().filter(|x| *x == "-e").count(), 1);
        assert!(a.contains(&"rg".to_string()));
    }

    // ── the userland's boxes (PLAN-phone-userland.md T3 design item 4) ──

    /// The userland makes the boxes where the config says so, and only
    /// there: it is never a fallback for a missing podman (its fence is
    /// weaker), and a config that names it on a node where it does not
    /// answer has no runtime rather than a quiet podman.
    #[test]
    fn the_runtime_follows_the_backend_and_what_this_node_has() {
        let ulsh = || Some(PathBuf::from("/lib/libulsh.so"));
        assert_eq!(
            runtime_for("userland", false, false, ulsh()),
            Runtime::Userland(PathBuf::from("/lib/libulsh.so"))
        );
        assert_eq!(
            runtime_for(" userland ", true, true, ulsh()),
            Runtime::Userland(PathBuf::from("/lib/libulsh.so")),
            "named, it wins over a podman too"
        );
        assert_eq!(runtime_for("userland", true, true, None), Runtime::None);
        assert_eq!(runtime_for("native", false, false, ulsh()), Runtime::None);
        assert_eq!(runtime_for("podman", false, true, ulsh()), Runtime::Podman);
        assert_eq!(
            runtime_for("native", true, false, None),
            Runtime::PodmanRemote
        );
    }

    /// proot is no network namespace, no capability set and no
    /// boundary: a userland box keeps the approval gate whatever else
    /// the section says.
    #[test]
    fn a_userland_box_is_never_fenced() {
        let mut s = crate::config::SandboxSection::default();
        assert!(section_is_fenced(&s), "the default podman box is");
        s.backend = "userland".into();
        assert!(!section_is_fenced(&s));
        s.network = false;
        assert!(!section_is_fenced(&s), "even asking for no network");
    }

    /// The background-shell policy (`bg_launch_argv`): podman detaches
    /// the runner inside one exec; in the userland that runner would die
    /// with its session (`--kill-on-exit`, and the broker's group kill on
    /// hang-up), so it is run in the foreground of a session held for
    /// the shell's whole life.
    #[test]
    fn a_userland_background_shell_is_held_by_its_session() {
        assert_eq!(
            bg_launch_argv(7, true),
            ["sh", "/work/.veles-bg/7/run.sh"],
            "held: the runner is the session, nothing detached"
        );
        let detached = bg_launch_argv(7, false);
        assert_eq!(detached[..2], ["sh", "-c"]);
        assert!(
            detached[2].ends_with("/work/.veles-bg/7/run.sh >/dev/null 2>&1 & echo $!"),
            "{detached:?}"
        );
        assert!(
            detached[2].starts_with("setsid -w sh "),
            "the returned runner pid must own the group cancel signals: {detached:?}"
        );
    }

    /// The exact podman launch shape owns a group, so killing its PGID
    /// reaches a grandchild that detached from the runner's stdin/out.
    /// This is a real process test, not a string-only assertion: the
    /// grandchild is due to write a canary after the runner is gone.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn podman_launch_and_group_cancel_reach_a_detached_grandchild() {
        let home = tempfile::tempdir().expect("scratch");
        let dir = home.path().join(".veles-bg/7");
        std::fs::create_dir_all(&dir).unwrap();
        let canary = home.path().join("canary");
        let child_pid = home.path().join("child.pid");
        std::fs::write(
            dir.join("run.sh"),
            format!(
                "(trap '' TERM; sleep 1; : > \"{}\") >/dev/null 2>&1 & echo $! > \"{}\"\n\
                 trap 'exit 143' TERM INT\nsleep 30 & wait $!\n",
                canary.display(),
                child_pid.display()
            ),
        )
        .unwrap();
        // No outer pre_exec group change: production's launch shape is
        // the only thing under test. A failed setup must name itself,
        // not look like a detached grandchild escaping cancellation.
        let mut launch = tokio::process::Command::new("sh");
        launch.arg("-c").arg(format!(
            "setsid -w sh {}/run.sh >/dev/null 2>&1 & echo $!",
            dir.display()
        ));
        let out = launch.output().await.expect("launch");
        assert!(out.status.success(), "{out:?}");
        let pid: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("runner pid");
        let start = std::time::Instant::now();
        while !child_pid.exists() && start.elapsed() < std::time::Duration::from_secs(2) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let child: i32 = std::fs::read_to_string(&child_pid)
            .unwrap_or_else(|e| {
                panic!(
                    "the launch fixture produced no grandchild pid after 2s: {e}; \
                     runner {pid}, run.sh={:?}, launch={out:?}",
                    std::fs::read_to_string(dir.join("run.sh"))
                )
            })
            .trim()
            .parse()
            .expect("pid");
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -TERM -{pid} 2>/dev/null || true"))
            .status()
            .expect("cancel");
        assert!(status.success());
        // The same escalation a dead shell gets from the host broker:
        // SIGTERM is not permission for a trap to hold the group open.
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -KILL -{pid} 2>/dev/null || true"))
            .status();
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        assert!(
            !canary.exists(),
            "detached grandchild {child} survived cancellation; runner {pid}"
        );
        if let Ok(fields) = std::fs::read_to_string(format!("/proc/{child}/stat")) {
            let state = fields
                .rsplit_once(") ")
                .and_then(|(_, rest)| rest.split_whitespace().next());
            assert_eq!(state, Some("Z"), "grandchild is still running: {fields}");
        }
    }

    /// A box's first command says where it ran; a broker that keeps no
    /// sandboxes ran it in the rootfs, and the box is refused there.
    #[test]
    fn a_box_whose_guest_does_not_say_its_name_is_refused() {
        assert!(userland_box_answer("k-1", true, "k-1", "").is_ok());
        let old = userland_box_answer("k-1", true, "", "")
            .unwrap_err()
            .to_string();
        assert!(old.contains("older than this veles"), "{old}");
        let other = userland_box_answer("k-1", true, "k-2", "")
            .unwrap_err()
            .to_string();
        assert!(other.contains("\"k-2\""), "{other}");
        let failed = userland_box_answer("k-1", false, "", "ulsh: no userland session: stopped")
            .unwrap_err()
            .to_string();
        assert!(failed.contains("stopped"), "{failed}");
        assert!(USERLAND_FIRST.contains("mkdir -p /work"));
        assert!(USERLAND_FIRST.contains(&format!("${}", crate::userland::SANDBOX_MARKER)));
    }

    /// The driver's runner finds the box's tclsh and names the fix when
    /// there is none, under the guest's own timeout.
    #[test]
    fn the_userland_tcl_runner_finds_tclsh_or_says_apk_add_tcl() {
        for want in [
            "command -v tclsh",
            "tclsh8.6",
            "apk add tcl",
            "exit 127",
            "exec timeout -k 5 \"$1\" \"$t\" \"$2\"",
        ] {
            assert!(USERLAND_TCLSH.contains(want), "{want}: {USERLAND_TCLSH}");
        }
    }

    /// Container paths are unix paths; the boxes are unix-only.
    #[cfg(unix)]
    #[test]
    fn container_path_semantics() {
        assert_eq!(container_resolve("~/f.txt"), "/work/f.txt");
        assert_eq!(container_resolve("~"), "/work");
        assert_eq!(container_resolve("src/main.rs"), "/work/src/main.rs");
        assert_eq!(container_resolve("/etc/passwd"), "/etc/passwd");
    }

    #[test]
    fn keys_are_stable_and_home_scoped() {
        // The key hashes VELES_HOME, so this needs the env guard even
        // though it never reads a config: without one it races whatever
        // scratch home a neighbouring test is holding, and "same id +
        // same home" fails between its own two calls.
        let (_home, _env) = crate::testenv::scratch_home();
        let a = key_for("irc:vau_eve");
        let b = key_for("irc:vau_eve");
        assert_eq!(a, b, "same id + same home ⇒ same key");
        assert!(a.starts_with("irc_vau_eve-"), "readable prefix: {a}");
        assert!(key_for("weird:id!with@chars") != key_for("weird:id!with@charz"));
        assert!(key_for("").starts_with("anon-"));
    }

    #[test]
    fn zone_routing_follows_the_ladder() {
        let (_home, _env) = crate::testenv::scratch_home();
        // No session ⇒ a local owner surface: the host unless the call
        // asks otherwise, or `local_default_zone` says otherwise.
        // Fencing the human at the keyboard out of their own machine by
        // default is not a posture, it is a bug.
        assert_eq!(zone_for(None, ZoneArg::Unset), Zone::Host);
        assert_eq!(zone_for(None, ZoneArg::Host), Zone::Host);
        assert!(matches!(zone_for(None, ZoneArg::Own), Zone::Sandbox { .. }));

        let mk = |role: crate::acl::Role, id: &str, sid: &str| {
            crate::acl::set_session_principal(sid, crate::acl::Principal::known(id, role));
        };

        // Below admin: ALWAYS sandboxed, args ignored — an explicit
        // `host: true` from the model buys nothing.
        for role in [
            crate::acl::Role::Guest,
            crate::acl::Role::User,
            crate::acl::Role::Trusted,
        ] {
            let sid = format!("s-{role:?}");
            mk(role, "irc:x", &sid);
            for arg in [ZoneArg::Unset, ZoneArg::Host, ZoneArg::Own] {
                assert!(
                    matches!(zone_for(Some(&sid), arg.clone()), Zone::Sandbox { .. }),
                    "{role:?} must be sandboxed even with {arg:?}"
                );
            }
            // A box they were never granted is IGNORED here (the tool
            // layer turns the request into a visible denial); they land
            // in their own instead of somewhere else's.
            let named = zone_for(Some(&sid), ZoneArg::Named("someone-elses".into()));
            assert!(
                !matches!(&named, Zone::Sandbox { name, .. } if name == "someone-elses"),
                "an ungranted name must not route: {named:?}"
            );
        }

        // Admin and owner over a connector: the SANDBOX by default
        // (USER: "use the sandbox by default"), with an explicit way
        // out — and any box they name.
        for role in [crate::acl::Role::Admin, crate::acl::Role::Owner] {
            let sid = format!("s-{role:?}");
            mk(role, "irc:boss", &sid);
            assert!(
                matches!(zone_for(Some(&sid), ZoneArg::Unset), Zone::Sandbox { .. }),
                "{role:?} defaults into a sandbox"
            );
            assert_eq!(
                zone_for(Some(&sid), ZoneArg::Host),
                Zone::Host,
                "{role:?} can opt out per call"
            );
            assert!(matches!(
                zone_for(Some(&sid), ZoneArg::Named("quarantine".into())),
                Zone::Sandbox { ref name, .. } if name == "quarantine"
            ));
        }
    }

    /// The session's box is what the routing follows, so two principals
    /// in one channel share one container and one workspace — the whole
    /// point of a shared sandbox.
    #[test]
    fn one_named_box_serves_everyone_routed_to_it() {
        let (_home, _env) = crate::testenv::scratch_home();
        crate::acl::set_session_principal(
            "share-a",
            crate::acl::Principal::known("irc:a", crate::acl::Role::User),
        );
        crate::acl::set_session_principal(
            "share-b",
            crate::acl::Principal::known("irc:b", crate::acl::Role::User),
        );
        crate::sandboxes::set_session_sandbox("share-a", "chan:irc/localtest/#veles");
        crate::sandboxes::set_session_sandbox("share-b", "chan:irc/localtest/#veles");
        let a = zone_for(Some("share-a"), ZoneArg::Unset);
        let b = zone_for(Some("share-b"), ZoneArg::Unset);
        assert_eq!(a, b, "same box, same container key");
        assert!(matches!(&a, Zone::Sandbox { name, .. } if name == "chan:irc/localtest/#veles"));
        // …and a third session in another channel does not.
        crate::acl::set_session_principal(
            "share-c",
            crate::acl::Principal::known("irc:c", crate::acl::Role::User),
        );
        crate::sandboxes::set_session_sandbox("share-c", "chan:irc/localtest/#other");
        assert_ne!(a, zone_for(Some("share-c"), ZoneArg::Unset));
    }

    /// `[sandbox] admin_default_zone = "host"` inverts the admin
    /// default without touching the below-admin fence.
    #[test]
    fn the_admin_default_zone_is_configurable() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[sandbox]\nadmin_default_zone = \"host\"\n",
        )
        .expect("write config");
        crate::config::invalidate_live();
        crate::acl::set_session_principal(
            "adz-admin",
            crate::acl::Principal::known("irc:boss", crate::acl::Role::Admin),
        );
        assert_eq!(zone_for(Some("adz-admin"), ZoneArg::Unset), Zone::Host);
        assert!(matches!(
            zone_for(Some("adz-admin"), ZoneArg::Own),
            Zone::Sandbox { .. }
        ));
        crate::acl::set_session_principal(
            "adz-user",
            crate::acl::Principal::known("irc:x", crate::acl::Role::User),
        );
        assert!(
            matches!(
                zone_for(Some("adz-user"), ZoneArg::Host),
                Zone::Sandbox { .. }
            ),
            "the knob is for admins; below admin there is no host path"
        );
        crate::config::invalidate_live();
    }

    /// `host` is the boolean, `sandbox` is the NAME, and anything
    /// malformed is UNSET rather than "host" — a typo must not open the
    /// fence.
    ///
    /// USER, 2026-09-09: *"I don't like that you have to specify
    /// `sandbox: true`, we should fail closed and default to sandbox.
    /// `host: true` should be the flag to execute on host."*
    #[test]
    fn the_zone_argument_reads_host_bools_and_sandbox_names() {
        use serde_json::json;
        assert_eq!(ZoneArg::from_args(&json!({})), ZoneArg::Unset);
        assert_eq!(ZoneArg::from_args(&json!({"host": true})), ZoneArg::Host);
        assert_eq!(ZoneArg::from_args(&json!({"host": false})), ZoneArg::Own);
        assert_eq!(
            ZoneArg::from_args(&json!({"sandbox": "shared"})),
            ZoneArg::Named("shared".into())
        );
        // Strings people actually type instead of a bool.
        assert_eq!(ZoneArg::from_args(&json!({"host": "yes"})), ZoneArg::Host);
        assert_eq!(
            ZoneArg::from_args(&json!({"host": "sandbox"})),
            ZoneArg::Own
        );
        // Junk is unset, never an escape.
        assert_eq!(ZoneArg::from_args(&json!({"host": 7})), ZoneArg::Unset);
        assert_eq!(
            ZoneArg::from_args(&json!({"host": "maybe"})),
            ZoneArg::Unset
        );
        assert_eq!(
            ZoneArg::from_args(&json!({"sandbox": "   "})),
            ZoneArg::Unset
        );
        // A NAME beats `host: true`. A call saying both is
        // contradictory and the fence is the fail-closed reading.
        assert_eq!(
            ZoneArg::from_args(&json!({"host": true, "sandbox": "quarantine"})),
            ZoneArg::Named("quarantine".into())
        );
        // The old spelling still parses, with its old polarity — a
        // skill or a memory written against the previous schema asking
        // for the fence must not silently get the host instead.
        assert_eq!(ZoneArg::from_args(&json!({"sandbox": true})), ZoneArg::Own);
        assert_eq!(
            ZoneArg::from_args(&json!({"sandbox": false})),
            ZoneArg::Host
        );
        assert_eq!(
            ZoneArg::from_args(&json!({"sandbox": "host"})),
            ZoneArg::Host
        );
        assert_eq!(ZoneArg::from_args(&json!({"sandbox": 7})), ZoneArg::Unset);
        // …and `host` wins over the legacy key when both are present.
        assert_eq!(
            ZoneArg::from_args(&json!({"host": false, "sandbox": false})),
            ZoneArg::Own
        );
    }

    /// A below-admin call's `host` request is removed from the
    /// arguments, so the transcript never records a host run that never
    /// happened — and a named box is left for the tool layer to deny
    /// out loud.
    #[test]
    fn a_denied_host_request_leaves_the_recorded_arguments() {
        use serde_json::json;
        let mut a = json!({"command": "id", "host": true});
        assert!(strip_host_request(&mut a));
        assert_eq!(a, json!({"command": "id"}));
        // Idempotent, and silent when there was nothing to strip.
        assert!(!strip_host_request(&mut a));
        let mut named = json!({"command": "id", "sandbox": "quarantine"});
        assert!(!strip_host_request(&mut named));
        assert_eq!(
            named,
            json!({"command": "id", "sandbox": "quarantine"}),
            "reachability is the tool layer's question, answered with a denial"
        );
        // Not an object (a malformed call): nothing to do, no panic.
        let mut junk = json!("id");
        assert!(!strip_host_request(&mut junk));
    }

    /// `[sandbox] local_default_zone = "sandbox"` fences the surfaces
    /// that carry no principal at all — the desktop, the TUI, the REPL,
    /// the bare CLI, cron and A2A.
    ///
    /// The DEFAULT is the host, which is the one deliberate exception
    /// to failing closed: reaching those surfaces already means being
    /// at this machine's keyboard.
    #[test]
    fn the_local_default_zone_is_configurable() {
        let (home, _env) = crate::testenv::scratch_home();
        assert_eq!(
            zone_for(None, ZoneArg::Unset),
            Zone::Host,
            "the local default is the host"
        );
        std::fs::write(
            home.path().join("config.toml"),
            "[sandbox]\nlocal_default_zone = \"sandbox\"\n",
        )
        .expect("write config");
        crate::config::invalidate_live();
        assert!(
            matches!(zone_for(None, ZoneArg::Unset), Zone::Sandbox { .. }),
            "the knob fences a call with no principal"
        );
        assert_eq!(
            zone_for(None, ZoneArg::Host),
            Zone::Host,
            "…and `host: true` is still the way out for the owner"
        );
        crate::config::invalidate_live();
    }

    /// A shared box is a shared RESOURCE, and the bound on it is per
    /// box: the people in one channel queue against each other, never
    /// against another channel. A caller who cannot get a slot is
    /// TOLD, with the knob named — a silent wait is the failure mode
    /// this whole file's honesty rules exist to prevent.
    #[tokio::test]
    async fn a_shared_box_bounds_its_concurrent_execs() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[sandbox]\nmax_concurrent_exec = 2\n",
        )
        .expect("write config");
        crate::config::invalidate_live();
        assert_eq!(max_concurrent_exec(), 2, "the limit is read live");

        let quick = Duration::from_millis(60);
        let first = acquire_exec_slot_within("boxa", quick)
            .await
            .expect("first slot");
        let second = acquire_exec_slot_within("boxa", quick)
            .await
            .expect("second slot");
        let refused = acquire_exec_slot_within("boxa", quick)
            .await
            .expect_err("a third concurrent exec has no slot");
        let msg = format!("{refused}");
        assert!(
            msg.contains("max_concurrent_exec") && msg.contains("boxa"),
            "the refusal names the box and the knob: {msg}"
        );
        assert!(
            acquire_exec_slot_within("boxb", quick).await.is_ok(),
            "another box has its own slots"
        );
        drop(first);
        assert!(
            acquire_exec_slot_within("boxa", quick).await.is_ok(),
            "a finished exec frees its slot"
        );
        drop(second);

        // 0 turns the bound off rather than blocking everything.
        std::fs::write(
            home.path().join("config.toml"),
            "[sandbox]\nmax_concurrent_exec = 0\n",
        )
        .expect("write config");
        crate::config::invalidate_live();
        assert!(
            matches!(acquire_exec_slot_within("boxa", quick).await, Ok(None)),
            "0 is unbounded"
        );
        crate::config::invalidate_live();
    }

    /// The bound counts the execs ACTUALLY in flight, so lowering it under
    /// a busy box takes effect on the next call.
    ///
    /// The semaphore this replaced was rebuilt when the limit changed, and
    /// permits held against the old one were invisible to the new one — so
    /// dropping `max_concurrent_exec` from 2 to 1 with two execs running
    /// admitted a third beside them. The knob exists to slow a box down,
    /// and the moment it is turned is the moment that answer is wrong.
    #[tokio::test]
    async fn lowering_the_exec_bound_counts_the_execs_already_running() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[sandbox]\nmax_concurrent_exec = 2\n",
        )
        .expect("write config");
        crate::config::invalidate_live();
        let quick = Duration::from_millis(60);

        let first = acquire_exec_slot_within("boxlower", quick)
            .await
            .expect("first slot")
            .expect("the bound is on");
        let second = acquire_exec_slot_within("boxlower", quick)
            .await
            .expect("second slot")
            .expect("the bound is on");
        assert!(
            acquire_exec_slot_within("boxlower", quick).await.is_err(),
            "two execs are in flight at a limit of 2"
        );

        // The knob drops to 1 while BOTH are still running.
        std::fs::write(
            home.path().join("config.toml"),
            "[sandbox]\nmax_concurrent_exec = 1\n",
        )
        .expect("write config");
        crate::config::invalidate_live();
        let err = acquire_exec_slot_within("boxlower", quick)
            .await
            .expect_err("two execs are in flight and the limit is now 1");
        assert!(
            format!("{err}").contains("max_concurrent_exec"),
            "the refusal still names the knob: {err}"
        );

        // Freeing one leaves the box AT the new limit, not under it.
        drop(first);
        assert!(
            acquire_exec_slot_within("boxlower", quick).await.is_err(),
            "one exec is still running and the limit is 1"
        );
        drop(second);
        assert!(
            acquire_exec_slot_within("boxlower", quick).await.is_ok(),
            "with both finished the new limit admits"
        );
        crate::config::invalidate_live();
    }

    /// A caller waiting for a slot is WOKEN by the exec that frees it
    /// rather than left to sit out its budget, and the wake reaches the
    /// box it belongs to: counting holders means there is no semaphore to
    /// hand a wake-up to, so the notification is explicit and worth a
    /// test of its own.
    #[tokio::test]
    async fn a_waiter_for_an_exec_slot_is_woken_by_the_exec_that_frees_it() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[sandbox]\nmax_concurrent_exec = 1\n",
        )
        .expect("write config");
        crate::config::invalidate_live();

        let held = acquire_exec_slot_within("boxwake", Duration::from_secs(5))
            .await
            .expect("the only slot")
            .expect("the bound is on");
        // A budget far longer than the wait below, so a lost wake fails on
        // the elapsed assertion instead of passing a race it happened to
        // win by timing out at the right moment.
        let waiter = crate::rt::spawn(async {
            acquire_exec_slot_within("boxwake", Duration::from_secs(8)).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter was admitted beside a slot that is still held"
        );

        let released = std::time::Instant::now();
        drop(held);
        let got = waiter.await.expect("join").expect("admitted");
        assert!(got.is_some(), "the bound is on, so this is a real slot");
        assert!(
            released.elapsed() < Duration::from_secs(5),
            "the waiter was not woken by the release — it sat out its budget: {:?}",
            released.elapsed()
        );
        crate::config::invalidate_live();
    }

    #[test]
    fn ledger_detects_external_change() {
        let before = b"alpha".to_vec();
        record_read("k", "/work/f", &before);
        assert!(!changed_since_read("k", "/work/f", &before));
        assert!(changed_since_read("k", "/work/f", b"beta"));
        // Unread paths are not stale — a first write is allowed.
        assert!(!changed_since_read("k", "/work/other", b"x"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn staging_guard_trips_on_symlink_escape() {
        let (home, _env) = crate::testenv::scratch_home();
        let root = home.path().join("user-envs");
        let work = root.join("k").join("work");
        std::fs::create_dir_all(&work).unwrap();
        // A symlink planted where the framework wants to stage: the
        // guard must refuse to use it.
        std::os::unix::fs::symlink("/etc", work.join(".veles-run")).unwrap();
        let res = fresh_stage_dir(&work, ".veles-run/123");
        assert!(res.is_ok(), "symlink is removed and recreated: {res:?}");
        let got = res.unwrap();
        assert!(got.starts_with(&work));
        // And the recreated dir is a REAL dir inside the workspace.
        assert!(got
            .canonicalize()
            .unwrap()
            .starts_with(work.canonicalize().unwrap()));
    }
}
