// An adapter/composition crate: its spawn sites are its OWN work (a
// connection task, the composition root driving a runtime), not a module
// calling into core. The M3 hop (clippy.toml) is core/frontends business.
#![allow(clippy::disallowed_methods)]

//! veles-cli — the composition root: core + adapters wired together.
//!
//! Lives outside veles-core so adapter crates (veles-telegram, later
//! veles-atproto) can depend on core without a circular dep through a
//! binary crate.

// The shared crates reach this crate through the `veles-runtime` dylib
// (PLAN-plugins-installer.md M1; crates/veles-cli/Cargo.toml says why).
// These aliases keep every `veles_core::…` / `veles_frontends::…` path
// as it was, and they are what makes rustc link the runtime.
use veles_runtime::veles_core;
use veles_runtime::veles_frontends;

pub mod extapi;
#[cfg(unix)]
pub mod modulehosttest;
#[cfg(unix)]
pub mod modulespike;
pub mod procplugins;
pub mod service;
pub mod tclrepl;

use std::sync::Arc;
use veles_frontends::gateway::Gateway;

/// Rust plugins built into this binary. Registered from `main` BEFORE
/// any surface boots an agent or the gateway: tools ride
/// `register_global_tool` (visible to sessions created after
/// registration) and hook names merge into the fire-gate, so both must
/// be in place before the first session. Add new in-tree plugins here.
///
/// The reference example (`rust_echo` and an observing hook) is opt-in
/// with `VELES_PLUGIN_EXAMPLE=1`: it is the template plugin authors copy,
/// not a tool every user's model should be offered.
pub fn register_builtin_rust_plugins() {
    if std::env::var("VELES_PLUGIN_EXAMPLE").is_ok_and(|v| v == "1") {
        veles_core::rustplugins::register_plugin(veles_plugin_example::plugin());
    }
}

/// The singleton fence as decided at boot, for the BOOT-GATED duties
/// (telegram, each IRC network). Cron is gated per
/// tick instead, in the gateway.
///
/// Three states, because two were a fail-OPEN: B3a returned `None` both
/// for "single node, run ungated" and for "the fence REFUSES", and the
/// duties read `None` as ungated. A fresh non-arbiter node — whose
/// registry has not heard of the arbiter until the first gossip lands —
/// logged "No cron, telegram, or IRC will start on this node" and
/// started telegram one line later (the 2026-09-28 two-gateway check).
#[derive(Clone)]
enum Fence {
    /// Nothing configured: single-node mode, the duties run ungated
    /// (there is no second node to double with).
    Ungated,
    /// Arbiter configured and resolvable: each duty must hold its lease.
    Armed(Arc<veles_core::arbiter::StaticArbiter>),
    /// A pool member whose fence cannot arm — no arbiter set, or one
    /// this node cannot resolve yet: no boot-gated duty starts here
    /// until a restart finds the arbiter.
    Refused,
}

/// The fence decision:
/// - arbiter configured and resolvable → ARMED;
/// - arbiter configured but unresolvable, or pool membership declared
///   (`node::pool_declared`) with no arbiter → REFUSED, loudly ("I have
///   peers but no arbiter" is the double-poller setup);
/// - nothing configured → UNGATED.
fn arm_singleton_fence(
    cfg: &Arc<veles_core::config::VelesConfig>,
    store: &Arc<std::sync::Mutex<veles_core::store::Store>>,
) -> Fence {
    let pool_declared = veles_core::node::pool_declared(&cfg.cluster);
    let arbiter_set = cfg
        .cluster
        .arbiter
        .as_deref()
        .is_some_and(|a| !a.trim().is_empty());
    match veles_core::arbiter::StaticArbiter::new(store.clone(), cfg.clone()) {
        Ok(arb) => {
            tracing::info!(
                "cluster: singleton fence ARMED (arbiter {}){}",
                arb.arbiter_id(),
                if arb.is_local() {
                    " — this node"
                } else {
                    " — remote"
                }
            );
            Fence::Armed(Arc::new(arb))
        }
        Err(e) if arbiter_set || pool_declared => {
            tracing::error!(
                "cluster: singleton fence REFUSES — {e:#}. No telegram or IRC will \
                 start on this node until a restart finds the arbiter (cron keeps \
                 trying on its own)."
            );
            Fence::Refused
        }
        Err(_) => Fence::Ungated,
    }
}

/// How a singleton duty runs on this node: not at all (the fence is
/// refused), ungated (single node — no lease exists to lose), or under
/// its lease's supervisor — started while this node holds the lease,
/// stopped the moment it is lost, started again when it comes back
/// (`LeaseGuard::supervise`).
enum SingletonStart {
    No,
    Ungated,
    Supervised(std::sync::Arc<veles_core::arbiter::LeaseGuard>),
}

/// How may the duty `what` run on this node? Logs the answer.
///
/// Armed: ask now, and keep asking for as long as the process runs —
/// holder or standby alike. A standby used to get no renewal loop and no
/// duty, so when the holder died nobody took the duty over, and a holder
/// that lost one renewal stayed silent until a restart.
async fn may_start_singleton(fence: &Fence, key: &str, me: &str, what: &str) -> SingletonStart {
    use veles_core::arbiter::LeaseGuard;
    match fence {
        Fence::Ungated => SingletonStart::Ungated,
        Fence::Refused => {
            tracing::error!("{what} not started: the singleton fence is refused on this node");
            SingletonStart::No
        }
        // A gateway that stops when idle (the phone) is no standby: it is
        // up minutes at a time, and asking the arbiter every 30 s for a
        // duty the VPS holds kept its radio awake for nothing. The cron
        // lease is skipped the same way (gateway.rs).
        Fence::Armed(_) if veles_core::idle::limit().is_some() => {
            tracing::info!(
                "cluster: {what} not started — this gateway stops when idle \
                 (--exit-when-idle), so it never stands by for lease {key}; a node that \
                 stays up holds it"
            );
            SingletonStart::No
        }
        Fence::Armed(arb) => {
            let guard = LeaseGuard::acquire(arb.as_ref(), key, me).await;
            guard.spawn_renewal(arb.clone());
            if guard.held() {
                tracing::info!("cluster: holding {what} (lease {key})");
            } else {
                tracing::warn!(
                    "cluster: {what} on standby — lease {key} not granted ({}); it starts \
                     here if this node is granted it later",
                    guard.lost_reason().unwrap_or_default()
                );
            }
            SingletonStart::Supervised(guard)
        }
    }
}

/// The singleton fence as the process-plugin host sees it: each
/// instance's lease goes through the SAME decision and supervisor as the
/// linked-in adapters', so the lease keys stay what they are and a pool
/// mixing old and new nodes still runs each platform once.
struct FenceGate {
    fence: Fence,
    me: String,
}

#[async_trait::async_trait]
impl veles_core::pluginhost::LeaseGate for FenceGate {
    async fn gated(
        &self,
        key: &str,
        what: &str,
        mut duty: veles_core::pluginhost::Duty,
    ) -> anyhow::Result<()> {
        match may_start_singleton(&self.fence, key, &self.me, what).await {
            SingletonStart::No => match self.fence {
                Fence::Refused => anyhow::bail!(
                    "the singleton fence is refused on this node (a pool member with no \
                     arbiter it can resolve); a restart that finds the arbiter starts it"
                ),
                _ => anyhow::bail!(
                    "this gateway stops when idle, so it never stands by for lease {key}; a \
                     node that stays up holds it"
                ),
            },
            SingletonStart::Ungated => duty().await,
            SingletonStart::Supervised(g) => g.supervise(what, duty).await,
        }
    }
}

/// Start the gateway and every configured platform adapter.
pub fn run_gateway() -> anyhow::Result<()> {
    // A panicked tokio task must take the process down, or systemd's
    // Restart=on-failure never fires and we serve broken. See
    // lifecycle::install_fatal_panic_hook for the incident this comes
    // from.
    veles_core::lifecycle::install_fatal_panic_hook();
    // Lifecycle: claim the pid file FIRST (fails loudly if another
    // gateway owns it), then record the boot and replay any
    // crash-loop warnings from the previous exit.
    veles_core::lifecycle::write_pid()?;
    for w in veles_core::lifecycle::record_start() {
        tracing::warn!("{w}");
    }
    let result = run_gateway_inner();
    veles_core::runsock::shutdown();
    match &result {
        Ok(()) if veles_core::idle::exited() => veles_core::lifecycle::record_stop("idle"),
        Ok(()) => veles_core::lifecycle::record_stop("clean"),
        Err(e) => veles_core::lifecycle::record_stop(&format!("error: {e:#}")),
    }
    veles_core::lifecycle::remove_pid();
    // A stop on idle has its own status, so the phone's supervisor
    // (GatewayService) leaves it stopped instead of restarting it.
    if result.is_ok() && veles_core::idle::exited() {
        std::process::exit(veles_core::idle::EXIT_CODE);
    }
    result
}

fn run_gateway_inner() -> anyhow::Result<()> {
    // The gateway is I/O-bound (adapters, HTTP APIs, Jetstream websockets,
    // the desktop surface, provider streams) — NOT CPU-bound. Tokio's
    // `Runtime::new()` default of worker_threads = num_cpus is sized for
    // CPU-bound work; on a small host (the VPS has 2 cores) that leaves 2
    // workers, so two tasks that block their thread (a CPU-bound turn, a
    // sync call that missed spawn_blocking) starve EVERYTHING else — the
    // atproto consumer stops claiming mentions and the desktop API hangs
    // at 100% (USER 2026-10-05). Oversubscribe: an I/O gateway wants
    // several workers per core so a blocked worker never idles the rest.
    // CPU-heavy work still belongs on spawn_blocking (the blocking pool is
    // separately sized and grows on demand).
    let workers = (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        * 4)
    .clamp(8, 32);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(64)
        .enable_all()
        .build()?;
    // Park core's runtime handle so `veles_core::rt::spawn` can hop MODULE-
    // thread work onto it (M3/R2): an ask's timeout started by a module
    // must outlive the module's instance runtime. Must precede any module
    // spawn; the first install wins.
    veles_core::rt::install_core_handle(rt.handle().clone());
    rt.block_on(async {
        let cfg = std::sync::Arc::new(veles_core::config::load()?);

        // MCP servers (M9): connect, discover tools, register them into
        // the DEFERRED registry (discoverable via tool_search, invocable
        // via tool_call — full schemas stay out of every system prompt).
        // One shared call: DesktopServer::new does the same thing, so
        // both must funnel through boot_mcp_deferred or tools register
        // twice. Per-server failures are logged and skipped — MCP must
        // never keep the gateway from booting.
        veles_core::mcp::boot_mcp_deferred(&cfg);

        let gateway = Gateway::new()?;
        let bus = gateway.bus();
        // This process's socket (session mail, PLAN-sessions-and-phone.md S2):
        // siblings sharing the data dir list the sessions it holds and
        // nudge one when mail arrives. Inside the runtime, so request
        // threads run their spawns on it.
        veles_core::runsock::serve("gateway");

        // B3a ordering: the fence validates [cluster] arbiter against
        // the registry, and a fresh node's own row is NOT there yet —
        // the heartbeat task registers it later, asynchronously. Arm
        // the fence only after registering ourselves SYNCHRONOUSLY, or
        // every first boot of the intended arbiter fails closed on
        // "matches no node in this registry" (the exact failure the
        // two-gateway check caught).
        {
            let store = gateway.store();
            match veles_core::node::resolve_self(&cfg).and_then(|me| {
                let s = store.lock().unwrap_or_else(|p| p.into_inner());
                veles_core::node::register_resolved(&s, &me).map(|_| me)
            }) {
                Ok(me) => tracing::info!(
                    "cluster: registered node {} ({}) [pre-fence]",
                    me.identity.id,
                    me.name
                ),
                Err(e) => tracing::warn!("cluster: pre-fence self-registration failed: {e:#}"),
            }
        }

        // The B3a singleton fence (one decision, then per-duty leases).
        let fence = arm_singleton_fence(&cfg, &gateway.store());
        let me_node_id =
            veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())?.id;

        // Failover (PLAN-pool-round.md C2): this node's liveness lease.
        // While it runs, the arbiter lets nobody take this node's
        // sessions over; when it lapses (the node died, or lost the
        // arbiter), a survivor may.
        if cfg.cluster.failover {
            match &fence {
                Fence::Armed(arb) => {
                    let key = veles_core::takeover::node_key(&me_node_id);
                    let guard =
                        veles_core::arbiter::LeaseGuard::acquire(arb.as_ref(), &key, &me_node_id)
                            .await;
                    guard.spawn_renewal(arb.clone());
                    if guard.held() {
                        tracing::info!("cluster: failover on — holding the node lease {key}");
                    } else {
                        tracing::warn!(
                            "cluster: failover on, but the node lease {key} is not held yet ({})",
                            guard.lost_reason().unwrap_or_default()
                        );
                    }
                    veles_core::takeover::set_node_lease(guard, cfg.cluster.failover_fence);
                }
                _ => tracing::warn!(
                    "cluster: [cluster] failover is set, but there is no arbiter to hold a node \
                     lease at — failover is off on this node"
                ),
            }
        }

        // Telegram runs as a MODULE (R6, crates/veles-module-telegram),
        // supervised by the module host below under `singleton:telegram`;
        // nothing links or starts it here. (IRC went the same way in R4.)

        // atproto/Bluesky is a PROCESS PLUGIN now (Phase 3), spawned by the
        // plugin host below from `<data>/process-plugins/atproto/` — no
        // longer linked into the gateway. A change to it reloads without
        // restarting core or the other platforms.

        // Tcl adapters (ircx etc.) land in M3 via the plugins interpreter;
        // the gateway loop below already dispatches their hook surface.

        // IRC — an IN-PROCESS MODULE now (PLAN-plugins-installer.md R4):
        // `veles-irc` is a dylib the module host below discovers, loads
        // and supervises (one instance per network, lease
        // `singleton:irc:{instance}` — the SAME key the linked adapter
        // held, so a mixed-version pool arbitrates the one connection).
        // The gateway no longer spawns a thread per network here; the
        // host's lease gate + supervisor replace it. What stays linked
        // is the rlib's config surface (`config_surface`/`env_surface`,
        // describing knobs, not starting anything) and the retired-admin
        // notice just below.

        // The owner presence (the bouncer) was removed on 2026-10-07: the
        // owner speaks through the bot. A config that still has its block
        // is told so by name rather than silently ignored.
        if let Some(why) = veles_irc::retired_admin_presence(&cfg) {
            tracing::warn!("{why}");
        }

        // Core's own services (the image describer the atproto plugin's
        // automatic alt text calls). Before any plugin starts, so the
        // first caller finds them.
        if let Err(e) = veles_core::vision::register_service() {
            tracing::warn!("vision.describe: {e}");
        }

        // Process plugins (PLAN-plugins-installer.md): each instance a
        // supervised child under its own lease. Nothing here can fail
        // the gateway — a plugin that cannot start is listed as refused
        // and noted. Started before the gateway drains its inbound queue,
        // so a platform's route exists when the first reply goes out.
        let plugins = veles_core::pluginhost::PluginHost::new(
            bus.clone(),
            &me_node_id,
            veles_core::pluginhost::HostOptions::default(),
        )
        .with_store(gateway.store());
        veles_core::pluginhost::install(plugins.clone());
        let started = plugins
            .start(
                &veles_core::pluginhost::default_roots(),
                &cfg.plugins.enabled,
                &cfg.plugins.disabled,
                veles_core::pluginhost::raw_config_json(),
                std::sync::Arc::new(FenceGate {
                    fence: fence.clone(),
                    me: me_node_id.clone(),
                }),
            )
            .await;
        if !started.is_empty() {
            tracing::info!("process plugins: {}", started.join(", "));
        }

        // In-process modules (PLAN-plugins-installer.md M2/M3, R3):
        // supervised dylibs on the shared runtime, each under its own
        // lease. Same posture as the process tier: a module that cannot
        // start is listed as refused, never fatal. `with_store` hands
        // every instance the gateway's store in its context (R4: a
        // platform module parks it for its own reads — IRC's `!history`).
        let modules = veles_core::modulehost::ModuleHost::new(
            bus.clone(),
            &me_node_id,
            veles_core::modulehost::HostOptions::default(),
        )
        .with_store(gateway.store());
        veles_core::modulehost::install(modules.clone());
        let started_modules = modules
            .start(
                &veles_core::modulehost::default_roots(),
                &cfg.plugins.enabled,
                &cfg.plugins.disabled,
                veles_core::pluginhost::raw_config_json(),
                std::sync::Arc::new(FenceGate {
                    fence: fence.clone(),
                    me: me_node_id.clone(),
                }),
            )
            .await;
        if !started_modules.is_empty() {
            tracing::info!("modules: {}", started_modules.join(", "));
        }

        println!(
            "veles gateway running — {} process plugin instance(s), {} module instance(s)",
            started.len(),
            started_modules.len()
        );
        let result = gateway.run().await;
        // Every plugin gets its shutdown (IRC its QUIT) before the
        // runtime drops and kill_on_drop takes the rest. Modules first:
        // their teardown needs the runtime intact to drain their gates.
        // 15 s, longer than the host's own 10 s runtime shutdown, which
        // a pinned engine (lang-lisp, lang-racket) always spends in full.
        // With both at 10 s they tied, and when this side won, the
        // teardown was dropped before it could log "kept mapped"
        // (2026-10-12).
        modules
            .shutdown_all(std::time::Duration::from_secs(15))
            .await;
        plugins
            .shutdown_all(std::time::Duration::from_secs(5))
            .await;
        result
    })
}

/// Env-mutating tests in this crate serialize on this.
///
/// There are only two, and they read each other's variables: the
/// `veles tcl` wire test sets `DESKTOP_BEARER_TOKEN` to prove the header
/// goes out, which is precisely the value the launcher hand-off test
/// resolves from the environment a moment later. Run in parallel (cargo's
/// default) the hand-off test read the other's token and failed on its
/// first assertion. veles-core has `testenv::lock()` for the same hazard;
/// it is `pub(crate)`, so the CLI brings its own.
///
/// Poison-recovering, for the same reason: a failed test must not cascade
/// into the one queued behind it.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The token to hand a child desktop process.///
/// An explicitly exported `VELES_TOKEN` wins — that is how a desktop
/// pointed at somebody else's gateway carries THAT gateway's credential
/// instead of the local one. Otherwise it is the local gateway's own,
/// which `desktop_api` mints into `<config_dir>/.env` on first boot; the
/// desktop cannot read that file itself (it is a Tcl process pointed at a
/// URL, with no idea which config dir the gateway resolved), so the
/// launcher is the hand-off. Empty means the API is open.
fn desktop_token_for_child() -> String {
    std::env::var("VELES_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(veles_core::authn::desktop_bearer_token)
}

/// Where the Tcl/Tk app lives.
///
/// Split out of `run_gui` because this chain had already rotted once, and
/// rotted in the direction that only shows up on a developer's box: the
/// XDG migration moved `veles_home()` from `~/.veles` to
/// `~/.config/veles`, which left the "dev-shell fallback" looking for
/// `~/.config/veles-agent/veles-desktop` — a directory that has never
/// existed. `veles gui` from a cargo build then died with "cannot find
/// the veles-desktop app" while the nix-wrapped binary worked, because
/// the wrapper sets `VELES_DESKTOP_DIR` (flake.nix:255). The deployment
/// on this box runs `target/debug/veles`, i.e. exactly the broken case,
/// and it was found by the last step of a plan about something else:
/// "confirm the desktop still attaches".
fn find_desktop_dir() -> Option<std::path::PathBuf> {
    // An explicit setting wins outright, INCLUDING when it is wrong: the
    // caller's "no main.tcl in …" is a better answer for a path someone
    // typed than silently using a different one and letting them wonder
    // which app they got.
    if let Some(dir) = std::env::var("VELES_DESKTOP_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return Some(std::path::PathBuf::from(dir));
    }
    desktop_dir_from(
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.to_path_buf())),
        Some(veles_core::config::user_home()),
    )
}

/// The candidate chain, pure: no `current_exe`, no environment, so a test
/// can point it at a fake layout instead of at whatever tree it happens
/// to be running in.
fn desktop_dir_from(
    exe_dir: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let found = |p: std::path::PathBuf| p.join("main.tcl").exists().then_some(p);

    // Beside the running binary and up to three levels above it: an
    // installed layout puts the app at `$out/veles-desktop` next to
    // `$out/bin/veles` (one up), a cargo build puts it at
    // `<repo>/veles-desktop` above `target/debug/veles` (two up), and a
    // nested target dir (`target/x86_64-unknown-linux-gnu/debug`) is
    // three.
    let mut dir = exe_dir;
    for _ in 0..4 {
        let Some(d) = dir else { break };
        if let Some(hit) = found(d.join("veles-desktop")) {
            return Some(hit);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    // A checkout in the home directory, which is how this project is
    // deployed on its own box: `~/veles-agent/veles-desktop`. This is the
    // fallback the stale `veles_home().parent()` arithmetic meant to be.
    home.and_then(|h| found(h.join("veles-agent").join("veles-desktop")))
}

/// The URL `veles gui` hands a FIRST-run window as `VELES_API`, or None
/// when the window has its own say: an explicit `VELES_API` or
/// `VELES_INSTANCE` in the environment, or a saved-instance registry
/// (`config::desktop_registry_path`, which the app writes the first time
/// something is attached). `port` is the local desktop API's.
fn first_run_api_url(port: u16) -> Option<String> {
    let set = |k: &str| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false);
    if set("VELES_API") || set("VELES_INSTANCE") {
        return None;
    }
    if veles_core::config::desktop_registry_path().exists() {
        return None;
    }
    Some(format!("http://127.0.0.1:{port}"))
}

/// Launch the veles-desktop Tcl/Tk window (`veles gui` / `veles desktop`).
///
/// Ensures the gateway's desktop API is reachable first (spawning
/// `veles gateway run` detached if not), then execs `wish main.tcl`.
pub fn run_gui() -> anyhow::Result<()> {
    use std::process::Command;
    use std::time::{Duration, Instant};

    // ── where is the desktop app? ────────────────────────────────────
    let desktop_dir = find_desktop_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot find the veles-desktop app: set VELES_DESKTOP_DIR (looked beside the \
             running binary and in $HOME/veles-agent)"
        )
    })?;
    let main_tcl = desktop_dir.join("main.tcl");
    if !main_tcl.exists() {
        anyhow::bail!("no main.tcl in {desktop_dir:?}");
    }

    // ── wish? ────────────────────────────────────────────────────────
    // `WISH` wins; else the bare name, else the versioned name a Tcl
    // built from source installs (`wish86t.exe` on Windows, `wish8.6`
    // on a distro that ships only that).
    let wish = std::env::var("WISH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ["wish", "wish86t", "wish8.6"]
                .iter()
                .find(|w| veles_core::shell::which(w).is_some())
                .map(|w| w.to_string())
        })
        .unwrap_or_else(|| "wish".to_string());

    // ── gateway up? spawn it if not ──────────────────────────────────
    let port = std::env::var("VELES_DESKTOP_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| veles_core::config::desktop_port(&veles_core::config::live()));
    let health = format!("http://127.0.0.1:{port}/api/health");
    if !desktop_health_matches(&health, Duration::from_millis(400)) {
        eprintln!(
            "veles gui: no gateway for profile {} responding on :{port} — starting it…",
            veles_core::profiles::active_profile()
        );
        let log = veles_core::config::veles_home().join("gui-gateway.log");
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let logf = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)?;
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(exe);
        cmd.arg("gateway")
            .arg("run")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(logf.try_clone()?))
            .stderr(std::process::Stdio::from(logf));
        // detach into its own process group (survives the exec below)
        veles_core::platform::own_group_std(&mut cmd);
        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("failed to start gateway: {e}"))?;
        // wait for the desktop API to come up
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if desktop_health_matches(&health, Duration::from_millis(300)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if !desktop_health_matches(&health, Duration::from_millis(300)) {
            anyhow::bail!(
                "gateway did not come up on :{port} (see {log:?}); run `veles gateway run` manually"
            );
        }
    }

    // ── exec the Tk app ──────────────────────────────────────────────
    //
    // Hand it the API's token. Read HERE, after the health check, because
    // the gateway this command may have just spawned is what mints it —
    // reading before the wait would hand over nothing on a cold start. A
    // window where every route answers 401 looks broken rather than
    // locked, and `main.tcl` already consumes VELES_TOKEN while `api.tcl`
    // already sends it (and names the variable when one is refused), so
    // this is the only missing link. See `desktop_token_for_child`.
    let token = desktop_token_for_child();
    if token.is_empty() {
        eprintln!(
            "veles gui: profile {} has no desktop API token — it is open to every process on loopback",
            veles_core::profiles::active_profile()
        );
    }
    // A FIRST run attaches to the gateway this command just made sure of.
    // The app's boot chain is `VELES_API > VELES_INSTANCE > the instance
    // last attached > unattached`, and on a machine with no saved
    // instance yet it ended in "unattached": a window holding a token
    // for a gateway it refuses to talk to, every pane empty, and nothing
    // the agent's `device` tool could reach (winbox, 2026-10-13: the
    // toast and `open` proofs failed on "no devices are connected" until
    // VELES_API was set by hand). With a registry in place the window
    // keeps choosing as before; this only names the loopback gateway
    // when there is nothing else to choose.
    let api_url = first_run_api_url(port);
    eprintln!("veles gui: launching {main_tcl:?} (wish: {wish})");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(&wish);
        cmd.arg(&main_tcl)
            .env("VELES_DESKTOP_PORT", port.to_string())
            // Force a UTF-8 locale so Tcl's system encoding is Unicode, not
            // iso8859-1 (its default when LANG is C/unset — the mojibake
            // root cause). api.tcl also forces UTF-8 on every HTTP read.
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8");
        if !token.is_empty() {
            cmd.env("VELES_TOKEN", &token);
        }
        if let Some(url) = &api_url {
            cmd.env("VELES_API", url);
        }
        // The binary that launched the window is the one it runs children
        // of (the ticket signer): `auto_execok veles` finds whatever is
        // first on PATH, and here that was an installed build a week older
        // than this one, with no `ticket` subcommand at all.
        if let Ok(exe) = std::env::current_exe() {
            cmd.env("VELES_EXE", exe);
        }
        let err = cmd.exec();
        anyhow::bail!("could not exec {wish}: {err}")
    }
    // No exec on Windows: run the window as a child and end with its
    // status, so `veles gui` from a console behaves the same.
    #[cfg(not(unix))]
    {
        let mut cmd = Command::new(&wish);
        cmd.arg(&main_tcl)
            .env("VELES_DESKTOP_PORT", port.to_string());
        if !token.is_empty() {
            cmd.env("VELES_TOKEN", &token);
        }
        if let Some(url) = &api_url {
            cmd.env("VELES_API", url);
        }
        if let Ok(exe) = std::env::current_exe() {
            cmd.env("VELES_EXE", exe);
        }
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("could not start {wish}: {e}"))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Health must identify the requested profile, not merely answer HTTP.
/// The default keeps accepting an older gateway with no profile field.
fn desktop_health_matches(url: &str, timeout: std::time::Duration) -> bool {
    let profile = veles_core::profiles::active_profile();
    if !http_ok(url, timeout) {
        return false;
    }
    let advertised = http_get_body(url, timeout)
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("profile")?.as_str().map(str::to_string));
    advertised.as_deref() == Some(profile.as_str())
        || (profile == "default" && advertised.is_none())
}

/// Minimal HTTP/1.0 GET — returns true on a 2xx response.
fn http_ok(url: &str, timeout: std::time::Duration) -> bool {
    use std::io::{Read, Write};
    use std::net::{IpAddr, TcpStream};
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let addr: IpAddr = host
        .split(':')
        .next()
        .unwrap_or("127.0.0.1")
        .parse()
        .unwrap_or("127.0.0.1".parse().unwrap());
    let port: u16 = host
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(80);
    let mut sock = match TcpStream::connect_timeout(&std::net::SocketAddr::new(addr, port), timeout)
    {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = sock.set_read_timeout(Some(timeout));
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    if sock.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match sock.read(&mut buf) {
        Ok(n) => buf[..n].starts_with(b"HTTP/1."),
        Err(_) => false,
    }
}

// ── gateway lifecycle operations (status / stop / restart / systemd) ──

/// `veles gateway status` — resolve the gateway's state from three
/// independent signals (systemd unit, pid file, desktop-API health) and
/// print a human summary plus the recent lifecycle records.
pub fn gateway_status() -> anyhow::Result<()> {
    use veles_core::lifecycle as lc;

    println!("veles gateway status");

    // 1. the service manager (systemd user unit, or the Windows logon task)
    let (line, unit_active) = service::status();
    println!("{line}");

    // 2. pid file
    let pid = lc::read_pid();
    let pid_running = pid.map(lc::is_running).unwrap_or(false);
    match pid {
        Some(p) if pid_running => println!("  pidfile: running (pid {p})"),
        Some(p) => println!("  pidfile: stale ({p}, not running)"),
        None => println!("  pidfile: none"),
    }

    // 3. desktop API health (uptime + version)
    //
    // The port the server actually binds, not a hardcoded 8642: with
    // VELES_DESKTOP_PORT set this used to report a DIFFERENT gateway's
    // uptime as though it were the one being asked about — and the auth
    // line below reads the config dir this command was pointed at, so the
    // two halves of one readout could describe two different servers.
    let api_port = veles_core::config::desktop_port(&veles_core::config::live());
    let health = http_get_body(
        &format!("http://127.0.0.1:{api_port}/api/health"),
        std::time::Duration::from_millis(800),
    );
    let health_ok = health.is_some();
    match health {
        Some(body) => {
            let uptime = body
                .split("uptime_s")
                .nth(1)
                .and_then(|s| s.split([':', ',']).nth(1))
                .and_then(|s| s.trim().parse::<u64>().ok());
            let version = body
                .split("\"version\":")
                .nth(1)
                .and_then(|s| s.split(['"', ',']).nth(1))
                .unwrap_or("?")
                .to_string();
            match uptime {
                Some(u) => println!("  api:      up (v{version}, {u}s)"),
                None => println!("  api:      responding (v{version})"),
            }
        }
        None => println!("  api:      unreachable on :{api_port}"),
    }

    // 4. the API's auth posture. A credential you cannot see is one you
    //    find out about from a 401 in some other tool, and the token is
    //    minted into a dotfile rather than printed — so say which posture
    //    is in force and where the credential lives.
    {
        let auth = veles_core::authn::from_env(
            veles_core::authn::DESKTOP_PEER_KEY,
            veles_core::authn::DESKTOP_BEARER_KEY,
        );
        let token = veles_core::authn::desktop_bearer_token();
        if auth.is_open() {
            if veles_core::authn::desktop_open_opted_out() {
                println!(
                    "  auth:     OPEN by choice ({}=1) — no token, loopback peers only",
                    veles_core::authn::DESKTOP_OPEN_KEY
                );
            } else {
                println!(
                    "  auth:     no token yet — the gateway mints one into {}/.env when it starts",
                    veles_core::config::config_dir().display()
                );
            }
        } else if token.is_empty() {
            println!(
                "  auth:     {} named peer token(s), no shared bearer",
                auth.peer_count()
            );
        } else {
            // Where the credential lives. NOT `std::env::var`: `config::load`
            // runs `load_env_file` at startup, which populates the process
            // environment from the same `.env` — so by the time a CLI
            // command prints anything, "it is in the environment" is true
            // every time and tells an operator nothing about which file to
            // edit. The file is the answer that is worth printing, and the
            // environment is what is left when the key is not in it.
            let env_file = veles_core::config::config_dir().join(".env");
            let key_prefix = format!("{}=", veles_core::authn::DESKTOP_BEARER_KEY);
            let in_vault = veles_core::vault::local_values()
                .contains_key(veles_core::authn::DESKTOP_BEARER_KEY);
            let in_file = std::fs::read_to_string(&env_file)
                .is_ok_and(|s| s.lines().any(|l| l.trim_start().starts_with(&key_prefix)));
            println!(
                "  auth:     bearer token ({} hex chars, from {}){}",
                token.len(),
                if in_vault {
                    format!("the vault, {}", veles_core::vault::vault_dir().display())
                } else if in_file {
                    env_file.display().to_string()
                } else {
                    "the environment".to_string()
                },
                match auth.peer_count() {
                    0 => String::new(),
                    n => format!(" + {n} peer token(s)"),
                }
            );
        }
    }

    let state = if unit_active || pid_running || lc::read_pid().is_some() || health_ok {
        "running"
    } else {
        "stopped"
    };
    println!("  state:    {state}");

    let records = lc::recent_records(6);
    if !records.is_empty() {
        println!("  lifecycle:");
        for r in records {
            println!("    {r}");
        }
    }
    Ok(())
}

/// `veles gateway stop` — through the service manager when it runs the
/// gateway; otherwise by pid: the IPC `shutdown` request (every
/// platform), SIGTERM where there is one, then the kill after 10 s.
pub fn gateway_stop() -> anyhow::Result<()> {
    if let Some(r) = service::stop() {
        return r;
    }
    gateway_stop_by_pid()
}

/// The pid-file half of a stop, shared with the Windows task path.
pub(crate) fn gateway_stop_by_pid() -> anyhow::Result<()> {
    use veles_core::lifecycle as lc;
    let Some(pid) = lc::read_pid() else {
        println!("veles gateway: not running (no pidfile)");
        return Ok(());
    };
    if !lc::is_running(pid) {
        println!("veles gateway: not running (stale pidfile)");
        let _ = std::fs::remove_file(lc::pid_file());
        return Ok(());
    }

    if lc::request_stop(pid) {
        println!("veles gateway: asked {pid} to stop");
    } else {
        println!("veles gateway: {pid} could not be asked; killing it");
        veles_core::platform::kill(pid);
    }
    // wait up to 10s for a clean exit, then the kill
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !lc::is_running(pid) {
            println!("veles gateway: stopped");
            return Ok(());
        }
    }
    println!("veles gateway: no response in 10s, killing {pid}");
    veles_core::platform::kill(pid);
    Ok(())
}

/// `veles gateway restart` — systemd restart if installed, else
/// stop + spawn a detached foreground gateway whose output lands in
/// the data home's `gateway.log` (`veles_home()`, i.e.
/// ~/.local/share/veles).
pub fn gateway_restart() -> anyhow::Result<()> {
    if let Some(r) = service::restart() {
        return r;
    }

    gateway_stop()?;
    let exe = std::env::current_exe()?;
    let home = veles_core::config::veles_home();
    let log = home.join("gateway.log");
    let log_f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("gateway")
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_f.try_clone()?))
        .stderr(std::process::Stdio::from(log_f));
    // Its own group (and, on Windows, no console of this one's).
    veles_core::platform::own_group_std(&mut cmd);
    let child = cmd.spawn();
    match child {
        Ok(c) => {
            println!(
                "veles gateway: restarted (pid {}, log {})",
                c.id(),
                log.display()
            );
        }
        Err(e) => anyhow::bail!("failed to spawn gateway: {e}"),
    }
    Ok(())
}

/// `veles gateway install` — a systemd user unit on unix, a logon task
/// on Windows (`service.rs`), enabled and started.
pub fn gateway_install() -> anyhow::Result<()> {
    service::install()
}

/// `veles gateway uninstall` — stop, disable, and remove it.
pub fn gateway_uninstall() -> anyhow::Result<()> {
    service::uninstall()
}

/// GET a URL and return the response body (best-effort, HTTP/1.0).
fn http_get_body(url: &str, timeout: std::time::Duration) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::{IpAddr, TcpStream};
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let addr: IpAddr = host
        .split(':')
        .next()
        .unwrap_or("127.0.0.1")
        .parse()
        .unwrap_or("127.0.0.1".parse().unwrap());
    let port: u16 = host
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(80);
    let mut sock =
        TcpStream::connect_timeout(&std::net::SocketAddr::new(addr, port), timeout).ok()?;
    let _ = sock.set_read_timeout(Some(timeout));
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    sock.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).to_string();
    // strip headers
    text.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

// ── veles setup ───────────────────────────────────────────────────────

/// Write a file that holds secrets, readable by its owner only.
///
/// A re-export: the implementation moved to `veles_core::setup` so the
/// desktop API writes its `.env` through the same code path (one writer,
/// one permission rule). Kept here because `main.rs`'s env-fill path and
/// this module's own history call it by this name.
pub fn write_private(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    veles_core::setup::write_private(path, text)
}

/// Read a secret: from the terminal with echo off, or one line from a
/// pipe. The value never lands on the screen, in the scrollback, or in
/// the shell's history (where it would as a command-line argument).
pub fn read_secret(prompt: &str) -> std::io::Result<String> {
    veles_core::platform::read_hidden(prompt)
}

/// `veles setup` — the headless first-run wizard.
///
/// A THIN FRONTEND over `veles_core::setup`: this function's whole job is
/// to collect answers on stdin, show the preview, and call `setup_plan`
/// then `apply`. The block builders, the config emitter, the write rule
/// and the 0600 rule all live in core, where the desktop's Setup… window
/// drives the same two functions — so the two wizards cannot emit
/// different TOML. That drift is what produced `api_key_env`,
/// `bot_token_env` and `[platforms.ircx]` (`60786dd`).
///
/// Over an existing config.toml every prompt defaults to what the file
/// already says, and the write EDITS it in place: pressing Enter
/// through the whole wizard changes nothing.
pub fn run_setup() -> anyhow::Result<()> {
    use std::io::{self, Write};
    use veles_core::setup::{AclChoice, ProviderChoice, SetupAnswers, SetupMode};

    // config.toml and .env live in the CONFIG dir since the XDG
    // migration — writing them to the data home meant a first-run
    // wizard whose output the gateway never read.
    let home = veles_core::config::config_dir();
    std::fs::create_dir_all(&home)?;
    let cfg_path = home.join("config.toml");

    println!("▲ veles setup — first-run wizard\n");

    let existing = veles_core::setup::read_existing(&cfg_path)?;
    let mut a = match existing.as_deref().filter(|t| !t.trim().is_empty()) {
        None => SetupAnswers::default(),
        Some(text) => {
            println!("Found existing config at {}", cfg_path.display());
            println!("It is EDITED in place: only the sections you answer change, and");
            println!("everything else in it — comments included — stays. Every prompt");
            println!("below defaults to what the file already says.");
            print!("Continue? [Y/n] ");
            io::stdout().flush()?;
            if read_line()?.eq_ignore_ascii_case("n") {
                println!("Nothing written.");
                return Ok(());
            }
            veles_core::setup::current_answers(text).map_err(|e| {
                anyhow::anyhow!(
                    "{} does not load, so the wizard cannot start from it: {e}",
                    cfg_path.display()
                )
            })?
        }
    };
    // Secret VALUES, by name, for the vault. Asked with echo off; an empty
    // answer keeps whatever is stored. They never go near config.toml.
    let mut values: std::collections::BTreeMap<String, String> = Default::default();
    let ask_secret = |name: &str, what: &str| -> anyhow::Result<Option<String>> {
        let stored = veles_core::secrets::resolve(name).is_some();
        let hint = if stored {
            "stored — Enter keeps it"
        } else {
            "Enter to add it later"
        };
        let v = read_secret(&format!("   {what} (${name}; it does not echo; {hint}): "))?;
        Ok((!v.is_empty()).then_some(v))
    };
    // A prompt's answer, or the default it showed when left empty.
    let ask = |label: &str, current: &str| -> anyhow::Result<String> {
        print!("   {label} [{current}]: ");
        io::stdout().flush()?;
        let v = read_line()?;
        Ok(if v.is_empty() { current.to_string() } else { v })
    };

    // ── 1. provider ─────────────────────────────────────────────────
    use veles_core::setup::{preset, PROVIDER_PRESETS};
    println!("\n1. LLM provider");
    for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
        if p.key_url.is_empty() {
            println!("   {}) {}", i + 1, p.label);
        } else {
            println!("   {}) {:<34} key: {}", i + 1, p.label, p.key_url);
        }
    }
    let skip_no = PROVIDER_PRESETS.len() + 1;
    println!("   {skip_no}) Skip — no provider entry (an existing one is left as it is)");
    let cur = PROVIDER_PRESETS
        .iter()
        .position(|p| p.choice == a.provider)
        .map_or(skip_no, |i| i + 1);
    let picked = loop {
        match ask("Choice", &cur.to_string())?.trim().parse::<usize>() {
            Ok(n) if (1..=skip_no).contains(&n) => break n,
            _ => println!("   pick a number from 1 to {skip_no}"),
        }
    };
    let chosen = PROVIDER_PRESETS
        .get(picked - 1)
        .map_or(ProviderChoice::Skip, |p| p.choice);
    if chosen != a.provider {
        // A different provider than the file had: its own defaults,
        // not the old entry's name, URL and key variable.
        a.provider_name.clear();
        a.base_url.clear();
        a.key_env.clear();
        a.api_mode.clear();
        a.driver.clear();
        a.model.clear();
        a.default_provider.clear();
    }
    a.provider = chosen;
    let or = |v: &str, d: &str| {
        if v.is_empty() {
            d.to_string()
        } else {
            v.to_string()
        }
    };
    // The key VALUE the model list may use: typed now, or already stored.
    let mut list_key = String::new();
    if let Some(p) = preset(a.provider) {
        a.provider_name = or(&a.provider_name, p.entry_name);
        a.base_url = or(&a.base_url, p.base_url);
        a.key_env = or(&a.key_env, p.key_env);
        a.api_mode = or(&a.api_mode, p.api_mode);
        if a.api_mode == "plugin" {
            a.driver = or(&a.driver, p.driver);
        }
        if matches!(
            a.provider,
            ProviderChoice::OpenAiCompatible | ProviderChoice::Ollama
        ) {
            a.base_url = ask("Base URL", &a.base_url)?;
        }
        if a.provider == ProviderChoice::OpenAiCompatible {
            // The NAME of a variable; the key itself is asked next and
            // goes into the vault. Empty: the server takes no key.
            a.key_env = ask(
                "Name for its API key (e.g. MYSERVER_API_KEY; empty = no key)",
                &a.key_env,
            )?;
        }
        if veles_core::secrets::valid_name(&a.key_env) {
            if !p.key_url.is_empty() {
                println!("   Get a key at {}", p.key_url);
            }
            if let Some(v) = ask_secret(&a.key_env, "The API key")? {
                list_key = v.clone();
                values.insert(a.key_env.clone(), v);
            } else if let Some(stored) = veles_core::secrets::resolve(&a.key_env) {
                list_key = stored;
            }
        }
        if ask("Advanced options (entry name, wire protocol)? (y/n)", "n")?
            .eq_ignore_ascii_case("y")
        {
            a.provider_name = ask("Provider entry name", &a.provider_name)?;
            a.api_mode = ask(
                "Wire protocol (chat_completions, responses or plugin)",
                &a.api_mode,
            )?;
            if a.api_mode == "plugin" {
                // The driver a plugin registers; `veles doctor --providers`
                // lists the loaded ones.
                a.driver = ask("Provider driver", &a.driver)?;
            }
        }
    }

    // ── 2. model + provider default ─────────────────────────────────
    println!("\n2. Default model");
    let suggested = preset(a.provider).map_or("", |p| p.model);
    let mut models: Vec<String> = Vec::new();
    if a.provider != ProviderChoice::Skip {
        println!("   Fetching the model list from {} …", a.base_url);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        match rt.block_on(veles_core::setup::list_models(&a.base_url, &list_key)) {
            Ok(m) if !m.is_empty() => models = m,
            Ok(_) => println!("   The provider listed no models; type the model's name."),
            Err(e) => println!("   Could not list models ({e}); type the model's name."),
        }
    }
    let show = |filter: &str| {
        let hits: Vec<(usize, &String)> = models
            .iter()
            .enumerate()
            .filter(|(_, m)| m.to_lowercase().contains(&filter.to_lowercase()))
            .collect();
        for (i, m) in hits.iter().take(60) {
            println!("   {:>4}) {m}", i + 1);
        }
        if hits.len() > 60 {
            println!("   … {} more — narrow it with ?text", hits.len() - 60);
        }
        if hits.is_empty() {
            println!("   nothing matches {filter:?}");
        }
    };
    if !models.is_empty() {
        if models.len() <= 60 || suggested.is_empty() {
            show("");
        }
        println!(
            "   {} models. Enter a number or a name; ?text searches the list.",
            models.len()
        );
    }
    let mut empty_answers = 0;
    a.model = loop {
        let v = ask("Model", &or(&a.model, suggested))?;
        let v = v.trim();
        if let Some(f) = v.strip_prefix('?') {
            show(f.trim());
            continue;
        }
        if let Ok(n) = v.parse::<usize>() {
            if let Some(m) = n.checked_sub(1).and_then(|i| models.get(i)) {
                break m.clone();
            }
        }
        if v.is_empty() && a.provider != ProviderChoice::Skip {
            empty_answers += 1;
            // A piped answer set that ran out answers empty forever; the
            // plan then refuses by name instead of this loop spinning.
            if empty_answers < 3 {
                println!("   a model is needed for the gateway to answer");
                continue;
            }
        }
        break v.to_string();
    };
    // The `[model]` table's provider — which entry ROUTES the default
    // model: the entry just written, unless the file routes elsewhere.
    a.default_provider = or(&a.default_provider, &a.provider_name);
    if a.provider == ProviderChoice::Skip {
        a.default_provider = ask("[model] provider", &a.default_provider)?;
    }

    // ── 3. telegram ─────────────────────────────────────────────────
    println!("\n3. Telegram (optional)");
    println!("   The bot token goes into the encrypted vault, never config.toml. Until");
    println!("   TELEGRAM_ALLOWED_USERS names your user id, anyone who finds the bot can");
    println!("   talk to it — as owner, with no [acl] section.");
    let yn = |b: bool| if b { "y" } else { "n" };
    a.telegram =
        ask("Enable the telegram adapter? (y/n)", yn(a.telegram))?.eq_ignore_ascii_case("y");
    if a.telegram {
        if let Some(v) = ask_secret("TELEGRAM_BOT_TOKEN", "The bot token from @BotFather")? {
            values.insert("TELEGRAM_BOT_TOKEN".into(), v);
        }
    }

    // ── 4. IRC ──────────────────────────────────────────────────────
    println!("\n4. IRC (optional; \"skip\" or empty = no IRC)");
    a.irc_server = ask("Server", &or(&a.irc_server, "skip"))?;
    if a.irc_server.eq_ignore_ascii_case("skip") {
        a.irc_server.clear();
    }
    if !a.irc_server.trim().is_empty() {
        if a.irc_port == 0 {
            a.irc_port = 6697; // a default the loop below can accept at EOF
        }
        a.irc_port = loop {
            // Re-asked, not guessed: the old prompt turned "66977" into
            // 6697 without a word.
            match ask("Port", &a.irc_port.to_string())?.parse::<u16>() {
                Ok(p) if p > 0 => break p,
                _ => println!("   the IRC port must be a number from 1 to 65535"),
            }
        };
        a.irc_use_tls = ask("TLS? (y/n)", yn(a.irc_use_tls))?.eq_ignore_ascii_case("y");
        a.irc_nickname = ask("Nickname", &or(&a.irc_nickname, "veles"))?;
        // A STRING, space or comma separated: `apply_mapping` reads the
        // key with `as_str`, so an array is ignored as quietly as a
        // missing key.
        a.irc_channels = ask(
            "Channels, e.g. \"#veles #dev\" (\"-\" = none)",
            &a.irc_channels,
        )?;
        if a.irc_channels.trim() == "-" {
            a.irc_channels.clear();
        }
        a.irc_sasl_username = ask(
            "SASL account for the nick (\"-\" = none)",
            &or(&a.irc_sasl_username, "-"),
        )?;
        if a.irc_sasl_username.trim() == "-" {
            a.irc_sasl_username.clear();
        }
        if !a.irc_sasl_username.trim().is_empty() {
            let name = veles_core::setup::IRC_SASL_PASSWORD;
            if let Some(v) = ask_secret(name, "Its SASL password")? {
                values.insert(name.into(), v);
            }
        }
    }

    // ── 5. ACL posture ──────────────────────────────────────────────
    println!("\n5. Access control");
    println!("   a) Legacy open — no [acl]; the adapter allowlist");
    println!("      (IRC_ALLOWED_USERS / TELEGRAM_ALLOWED_USERS) is the");
    println!("      trust boundary and every sender resolves as owner.");
    println!("      An existing [acl] is left as it is.");
    println!("   b) Curated — write the first [acl] owner. This switches the");
    println!("      gateway into curated mode: the default role becomes guest");
    println!("      (fail-closed).");
    let cur = if a.acl == AclChoice::Curated {
        "b"
    } else {
        "a"
    };
    if ask("Choice", cur)?.eq_ignore_ascii_case("b") {
        a.acl = AclChoice::Curated;
        a.acl_owner = ask(
            "Owner (telegram:123456, irc:account, or a nick!user@host mask)",
            &a.acl_owner,
        )?;
    } else {
        a.acl = AclChoice::LegacyOpen;
    }

    // ── 6. preview, then write ──────────────────────────────────────
    // The same preview the desktop shows: the exact bytes about to land
    // (as a diff over an existing file), plus the honest notes.
    let plan = veles_core::setup::setup_plan(&a, existing.as_deref())?;
    match plan.mode {
        SetupMode::Fresh => {
            println!("\n─── preview: {} ───", cfg_path.display());
            print!("{}", plan.config_toml);
        }
        SetupMode::Edit => {
            let diff = veles_core::setup::preview_diff(
                existing.as_deref().unwrap_or(""),
                &plan.config_toml,
            );
            if diff.is_empty() {
                println!("\nNo change to {}.", cfg_path.display());
            } else {
                println!("\n─── changes to {} ───", cfg_path.display());
                print!("{diff}");
            }
        }
    }
    if !plan.notes.is_empty() {
        println!("─── notes ───");
        for n in &plan.notes {
            println!("  · {n}");
        }
    }
    // Only values for names these answers still use (a key typed before
    // the IRC step was switched off is dropped, never stored).
    values.retain(|k, _| plan.secrets.contains(k));
    if !plan.secrets.is_empty() {
        println!("─── secrets (names only — values are never shown) ───");
        for n in &plan.secrets {
            let state = if values.contains_key(n) {
                "will be stored in the encrypted vault"
            } else if veles_core::secrets::resolve(n).is_some() {
                "already stored"
            } else {
                "not set yet — `veles secret set` it later"
            };
            println!("  ${n}: {state}");
        }
    }
    print!("\nWrite this? [y/N] ");
    io::stdout().flush()?;
    if !read_line()?.eq_ignore_ascii_case("y") {
        println!("Nothing written.");
        return Ok(());
    }

    let report = veles_core::setup::apply_with(&plan, &home, &values)?;
    if let Some(bak) = &report.config_backup {
        println!("✓ Kept the previous config as {}", bak.display());
    }
    println!("✓ Wrote {}", report.config_path.display());
    if !report.secrets_stored.is_empty() {
        println!(
            "✓ Stored {} in the encrypted vault ({}) — the agent can use a provider \
             key in commands as $NAME without ever seeing it",
            report.secrets_stored.join(", "),
            veles_core::vault::vault_dir().display()
        );
    }
    if report.env_written {
        println!(
            "✓ Noted {} in {} (commented lines, for reference)",
            report.env_added.join(", "),
            report.env_path.display()
        );
    }

    println!("\nNext steps:");
    let mut step = 1;
    if !report.secrets_needed.is_empty() {
        for n in &report.secrets_needed {
            println!("  {step}. `veles secret set {n}` — it prompts without echo");
            step += 1;
        }
    }
    println!("  {step}. `veles` — chat in this terminal");
    println!(
        "  {}. `veles gui` — the desktop app (it starts the gateway itself)",
        step + 1
    );
    println!(
        "  {}. `veles gateway install` — keep the gateway running as a user service \
         (needed for Telegram/IRC and scheduled jobs)",
        step + 2
    );
    Ok(())
}

/// The interactive chat surfaces on a box with no config.toml yet: on a
/// terminal, offer the setup wizard (a first run used to end in "no
/// model.provider in config"); off one, refuse with what to run.
/// `Ok(true)` = a config now exists, go on; `Ok(false)` = the person
/// declined, stop quietly.
pub fn first_run_gate(interactive: bool) -> anyhow::Result<bool> {
    use std::io::Write;
    let cfg = veles_core::config::config_dir().join("config.toml");
    if cfg.exists() {
        return Ok(true);
    }
    if !interactive {
        anyhow::bail!(
            "veles is not set up yet: run `veles setup` (terminal) or `veles gui` (desktop) first"
        );
    }
    println!(
        "Welcome to veles — there is no configuration yet ({}).",
        cfg.display()
    );
    print!("Run the setup wizard now? [Y/n] ");
    std::io::stdout().flush()?;
    if read_line()?.eq_ignore_ascii_case("n") {
        println!("Run `veles setup` (or `veles gui`) when you are ready.");
        return Ok(false);
    }
    run_setup()?;
    Ok(cfg.exists())
}

/// One trimmed line from stdin ("" at EOF), for the wizard's prompts.
fn read_line() -> anyhow::Result<String> {
    use std::io::{self, BufRead, Write};
    let mut s = String::new();
    if io::stdin().lock().read_line(&mut s)? == 0 {
        // EOF (a piped answer set that ran out): answer empty rather than
        // loop forever printing prompts at a closed pipe.
        io::stdout().flush()?;
        return Ok(String::new());
    }
    Ok(s.trim().to_string())
}

// ── veles model ───────────────────────────────────────────────────────

/// `veles model` — list available models or set the default.
/// Usage:
///   veles model              — show current model + list
///   veles model <name>       — set default model
///   veles model --provider p — set provider for the model
pub fn run_model(args: &[String]) -> anyhow::Result<()> {
    // The config root, not the data home — `veles model x` used to
    // look for config.toml where the XDG migration moved it FROM.
    let home = veles_core::config::config_dir();
    let cfg_path = home.join("config.toml");

    if !cfg_path.exists() && veles_core::config::hermes_config_yaml().is_none() {
        anyhow::bail!("no config.toml and no hermes config.yaml — run `veles setup` first");
    }

    if args.is_empty() {
        // Show current
        let cfg = veles_core::config::load()?;
        let model = cfg.model.default.as_deref().unwrap_or("(unset)");
        let provider = cfg.model.provider.as_deref().unwrap_or("(unset)");
        println!("current model: {model}");
        println!("current provider: {provider}");
        println!("\navailable providers:");
        if cfg.custom_providers.is_empty() {
            println!("  (none — add [[custom_providers]] entries to config.toml)");
        }
        for p in &cfg.custom_providers {
            println!("  {} ({})", p.name, p.base_url);
        }
        if !cfg.model_aliases.is_empty() {
            println!("\naliases:");
            for (name, a) in &cfg.model_aliases {
                println!("  {name} -> {} @ {}", a.model, a.provider);
            }
        }
        return Ok(());
    }

    // Set the default — through `/defaults model`'s resolver and its one
    // `configedit` write, so `veles model x`, `/defaults model x` and the
    // desktop's Save as default mean the same thing and touch the same
    // two keys. This used to be its own line editor over the file
    // (2026-10-04 defaults review: five writers for `[model]`).
    if !cfg_path.exists() {
        anyhow::bail!(
            "no config.toml at {} — run `veles setup` first",
            cfg_path.display()
        );
    }
    let new_model = &args[0];
    let new_provider = args
        .iter()
        .position(|a| a == "--provider")
        .and_then(|i| args.get(i + 1));
    let spec = match new_provider {
        Some(p) => format!("{new_model}@{p}"),
        None => new_model.clone(),
    };
    let cfg = veles_core::config::load()?;
    match veles_frontends::commands::defaults_model(&cfg, &spec) {
        Ok(report) => {
            println!("{report}");
            Ok(())
        }
        Err(e) => anyhow::bail!("{e}"),
    }
}

// ── veles backup / restore ────────────────────────────────────────────

/// `veles backup [--dir <dest>]` — snapshot the brain.
pub fn run_backup(dest_override: Option<&std::path::Path>) -> anyhow::Result<()> {
    // BOTH roots: the brain has lived in two places since the XDG
    // migration, and a backup of one root is half a brain wearing
    // a checkmark.
    let data = veles_core::config::veles_home();
    let config = veles_core::config::config_dir();
    let dest = match dest_override {
        Some(d) => d.to_path_buf(),
        None => data.join("backups").join(
            chrono::Local::now()
                .format("veles-%Y%m%d_%H%M%S")
                .to_string(),
        ),
    };
    let report = veles_core::backup::backup_to(&config, &data, &dest)?;
    println!("✓ brain backed up to {}", report.dest.display());
    println!(
        "  state.db: {:.1} MB",
        report.db_bytes as f64 / (1024.0 * 1024.0)
    );
    println!("  files: {}", report.files);
    println!("  restore with: veles restore {}", report.dest.display());
    Ok(())
}

/// `veles restore <dir>` — restore a backup (refuses while gateway runs).
pub fn run_restore(src: &std::path::Path) -> anyhow::Result<()> {
    let data = veles_core::config::veles_home();
    let config = veles_core::config::config_dir();
    let report = veles_core::backup::restore_from(&config, &data, src)?;
    println!("✓ restored from {}", report.src.display());
    if let Some(aside) = &report.moved_aside {
        println!("  previous brain moved to {}", aside.display());
    }
    println!("  files: {}", report.files);
    if !report.memory_folded.is_empty() {
        println!(
            "  memory: {} file(s) from the backup's memory.d folded into the database",
            report.memory_folded.len()
        );
    }
    Ok(())
}

// ── veles memory ──────────────────────────────────────────────────────

/// `veles memory ls [prefix]` — one line per live file.
pub fn memory_ls(
    s: &veles_core::memory::MemoryStore,
    prefix: Option<&str>,
) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    let files: Vec<_> = s
        .list()?
        .into_iter()
        .filter(|f| prefix.is_none_or(|p| f.path.starts_with(p)))
        .collect();
    let width = files.iter().map(|f| f.path.len()).max().unwrap_or(0);
    let mut out = String::new();
    for f in &files {
        let _ = writeln!(
            out,
            "{:<width$}  {:>7}  r{:<5} {}  [{}]",
            f.path, f.chars, f.rev, f.at, f.source
        );
    }
    let total: usize = files.iter().map(|f| f.chars).sum();
    let _ = writeln!(out, "{} file(s), {total} chars", files.len());
    Ok(out)
}

/// A revision as a person types it: `r12` or `12`.
pub fn parse_rev(s: &str) -> anyhow::Result<i64> {
    s.trim()
        .trim_start_matches('r')
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("not a revision: {s:?} (want r12 or 12)"))
}

/// The editor a person asked for: `$VISUAL`, then `$EDITOR`, then `vi`.
pub fn editor_command() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// `veles memory edit <path>` — the replacement for opening
/// `memory.d/<path>` in an editor, which is gone with memory.d.
///
/// The content goes to a private temp file (0600, in
/// `$XDG_RUNTIME_DIR` — a per-user tmpfs — when there is one), the
/// editor runs through `sh -c` so `EDITOR="code -w"` works, and the
/// result is saved with [`MemoryStore::replace_if`] against the sha the
/// edit started from. If the agent wrote the file meanwhile nothing is
/// overwritten: the error says so and the temp file is KEPT with your
/// text in it. Unchanged text records nothing.
///
/// [`MemoryStore::replace_if`]: veles_core::memory::MemoryStore::replace_if
pub fn run_memory_edit(
    s: &veles_core::memory::MemoryStore,
    path: &str,
    editor: &str,
) -> anyhow::Result<()> {
    let path = veles_core::memory::MemoryStore::check_rel(path)?;
    let (base, before) = match s.file(path)? {
        Some((f, content)) => (Some(f.sha256), content),
        None => (None, String::new()),
    };
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    // A clock timestamp is not a unique name: independent edits of the
    // same path can start in one tick. NamedTempFile creates the file
    // atomically, and keep() preserves the user's text on every recovery
    // path below. The directory still has the private-runtime preference.
    let tmp = tempfile::Builder::new()
        .prefix("veles-memory-")
        .tempfile_in(&dir)
        .map_err(|e| {
            anyhow::anyhow!(
                "create a private memory edit file in {}: {e}",
                dir.display()
            )
        })?;
    let tmp = tmp.into_temp_path();
    veles_core::platform::restrict_file(&tmp)
        .map_err(|e| anyhow::anyhow!("secure {}: {e}", tmp.display()))?;
    std::fs::write(&tmp, before.as_bytes())
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
    // Recovery messages intentionally retain this file for the user.
    // Other successful paths remove it explicitly below.
    let tmp = tmp
        .keep()
        .map_err(|e| anyhow::anyhow!("keep {}: {e}", e.path.display()))?;

    let status = veles_core::platform::editor_command(editor, &tmp)
        .status()
        .map_err(|e| anyhow::anyhow!("run the editor ({editor}): {e}"))?;
    if !status.success() {
        anyhow::bail!(
            "the editor ({editor}) exited with {status}; nothing recorded — your text is in {}",
            tmp.display()
        );
    }
    let after = std::fs::read_to_string(&tmp)?;
    if after == before {
        let _ = std::fs::remove_file(&tmp);
        println!("{path}: no change");
        return Ok(());
    }
    if after.is_empty() {
        anyhow::bail!(
            "{path} was saved empty — to delete it use `veles memory rm {path}`; \
             nothing recorded, the temp file is {}",
            tmp.display()
        );
    }
    match s.replace_if(path, &after, base.as_deref(), "edit") {
        Ok(_) => {
            let _ = std::fs::remove_file(&tmp);
            println!("{path}: recorded");
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(
            "{e:#} — nothing was overwritten; your text is in {}",
            tmp.display()
        )),
    }
}

// ── veles tool ────────────────────────────────────────────────────────

/// `veles tool --name X --args JSON` — run ONE tool against the real
/// registry and print its result. This is the execute_code harness's
/// bridge: Tcl scripts call back through here to reach the real tool
/// implementations.
///
/// Deliberately base_registry (no cronjob, no delegate, no clarify) —
/// the same recursion-guard posture as cron job sessions, so a harness
/// script cannot schedule jobs or spawn agents. `full` escalates to
/// the session registry, which is what an operator's own shell drives
/// (`veles tool --full`, the B4 two-gateway check's placement path);
/// nothing in the harness passes it.
pub fn run_tool(name: &str, args_json: &str, full: bool) -> anyhow::Result<()> {
    let home = veles_core::config::veles_home();
    let store = std::sync::Arc::new(std::sync::Mutex::new(veles_core::store::Store::open(
        &home.join("state.db"),
    )?));
    // base_registry is the cron-job posture (no cronjob, no delegate,
    // no clarify) so a harness script cannot schedule work or spawn
    // agents by accident. `--full` is the explicit escalation to the
    // registry a session gets — what `veles chat -q` could already
    // reach, which is why it is a flag and not a different verb.
    let registry = if full {
        veles_core::tools::builtin_registry(store, None, "cli-tool")
    } else {
        veles_core::tools::base_registry(store, "cli-tool")
    };
    let args: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| anyhow::anyhow!("--args is not valid JSON: {e}"))?;
    let rt = tokio::runtime::Runtime::new()?;
    let out = rt.block_on(registry.dispatch(name, args));
    println!("{out}");
    Ok(())
}

// ── veles import-hermes ───────────────────────────────────────────────

/// `veles import-hermes` — one-shot migration of hermes session history
/// into the veles store. Idempotent, refuses while the gateway runs,
/// opens the hermes DB strictly read-only.
pub fn run_import_hermes(
    db: Option<&std::path::Path>,
    source: Option<&str>,
    since_days: Option<i64>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let home = veles_core::config::veles_home();
    let hermes_db = match db {
        Some(p) => p.to_path_buf(),
        None => {
            // Default: ~/.hermes/state.db via the real home dir (NOT
            // derived from VELES_HOME — scratch homes would point at a
            // scratch .hermes that doesn't exist).
            directories::UserDirs::new()
                .map(|d| d.home_dir().join(".hermes/state.db"))
                .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?
        }
    };
    if !hermes_db.is_file() {
        anyhow::bail!("hermes state.db not found at {}", hermes_db.display());
    }
    let store = veles_core::store::Store::open(&home.join("state.db"))?;
    let results = veles_core::hermes_import::run(&store, &hermes_db, source, since_days, dry_run)?;

    let mut imported = 0usize;
    let mut msgs = 0usize;
    let mut skipped = 0usize;
    for r in &results {
        match &r.status {
            veles_core::hermes_import::Status::Imported { messages } => {
                imported += 1;
                msgs += messages;
                println!(
                    "  {} [{}] {} msgs{}",
                    if dry_run { "would import" } else { "imported" },
                    r.session.source,
                    messages,
                    title_note(r.session.title.as_deref()),
                );
                if dry_run {
                    // dry-run counts messages as 0 (nothing read); show
                    // the candidate count instead.
                }
            }
            veles_core::hermes_import::Status::SkippedExists => {
                skipped += 1;
            }
            veles_core::hermes_import::Status::SkippedEmpty => {}
        }
    }
    if dry_run {
        let candidates: usize = results
            .iter()
            .filter(|r| matches!(r.status, veles_core::hermes_import::Status::Imported { .. }))
            .map(|r| r.session.message_count)
            .sum();
        println!(
            "dry run: would import {imported} session(s), {candidates} messages ({skipped} already present)"
        );
    } else {
        println!("✓ imported {imported} session(s), {msgs} messages ({skipped} already present)");
    }
    Ok(())
}

fn title_note(title: Option<&str>) -> String {
    match title {
        Some(t) => {
            let t = t.chars().take(60).collect::<String>();
            format!(" — {t}")
        }
        None => String::new(),
    }
}

// ── veles migrate ─────────────────────────────────────────────────────

/// `veles migrate` — create config.toml in the config home
/// ($XDG_CONFIG_HOME/veles) from the hermes config.yaml, so the brain
/// can cut over without reconfiguring.
/// Non-destructive: backs up existing config.toml to .bak.
pub fn run_migrate() -> anyhow::Result<()> {
    // The hermes cutover writes into the CONFIG directory: config.toml
    // is what it produces.
    let home = veles_core::config::config_dir();
    // A cutover is usually the FIRST thing run on a machine, so the
    // config dir need not exist yet. Without this the command died on the write with
    // a bare "No such file or directory (os error 2)" — in exactly the
    // situation it exists for.
    std::fs::create_dir_all(&home)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", home.display()))?;
    let dest = home.join("config.toml");

    // Source: hermes config.yaml
    let src = veles_core::config::hermes_config_yaml().ok_or_else(|| {
        anyhow::anyhow!("no hermes config.yaml found — run `veles setup` for a fresh config")
    })?;
    let raw = std::fs::read_to_string(&src)?;
    let (cfg, report) = veles_core::config::from_hermes_yaml_report(&raw)?;

    // Serialize as TOML
    let toml_str = toml::to_string_pretty(&cfg)
        .map_err(|e| anyhow::anyhow!("TOML serialization failed: {e}"))?;

    // Backup if destination exists
    if dest.exists() {
        let bak = dest.with_extension("toml.bak");
        std::fs::rename(&dest, &bak)?;
        println!("Backed up existing config.toml to {}", bak.display());
    }

    std::fs::write(&dest, &toml_str)?;
    println!("✓ Migrated hermes config.yaml → {}", dest.display());
    println!("  Source: {}", src.display());

    // Say what happened to every key that was not a straight copy.
    //
    // Serde ignores unknown fields, so this used to print a tick over a
    // config that had silently lost settings — and the user then ran on
    // defaults they never chose with no way to discover it.
    if !report.translated.is_empty() {
        println!();
        println!("Renamed on the way across:");
        for (from, to) in &report.translated {
            println!("  {from}  →  {to}");
        }
    }
    if !report.unsupported.is_empty() {
        println!();
        println!("NOT carried over — veles has no equivalent:");
        for (key, why) in &report.unsupported {
            println!("  {key}: {why}");
        }
    }
    if !report.ignored.is_empty() {
        println!();
        println!(
            "Ignored (hermes bookkeeping): {}",
            report.ignored.join(", ")
        );
    }

    println!();
    println!("The config is now veles-native (config.toml). The hermes");
    println!("config.yaml is no longer read once config.toml exists.");
    println!();
    println!("Config is only part of a cutover. What lives outside it:");
    println!("  veles migrate-data      skills, cron jobs and usage history");
    println!("  veles import-hermes     session transcripts");
    println!();
    println!("Next steps:");
    println!("  1. Review the migrated config: {}", dest.display());
    println!("  2. Run `veles doctor` to verify");
    println!("  3. Run `veles gateway run` to start the veles gateway");

    Ok(())
}

// ── veles migrate-data ────────────────────────────────────────────────

/// `veles migrate-data` — the hermes state that config.yaml does not
/// carry: skills, cron jobs, usage history.
///
/// THE POINT IS SKILLS. `skills::skill_roots` lists `~/.hermes/skills`
/// as a live root, so a skill that exists only there is in every
/// session's prompt index and deleting the hermes install silently
/// removes it. `veles migrate` moved the config and reported success
/// while that dependency stood, which made "migrated" mean less than it
/// sounded.
///
/// Never writes to ~/.hermes. Idempotent — anything already present on
/// the veles side is left exactly as it is, and re-running says so.
pub fn run_migrate_data(dry_run: bool, only: Option<&str>) -> anyhow::Result<()> {
    let home = veles_core::config::veles_home();
    let steps: &[&str] = match only {
        None => &["skills", "cron", "usage"],
        Some(s) => match s {
            "skills" => &["skills"],
            "cron" => &["cron"],
            "usage" => &["usage"],
            other => anyhow::bail!("unknown step `{other}` (skills | cron | usage)"),
        },
    };

    if dry_run {
        println!("dry run — nothing will be written\n");
    }

    // Only opened when a step needs it: `--only skills` should work on a
    // machine whose store has never been created.
    let store = if steps.iter().any(|s| *s == "cron" || *s == "usage") {
        Some(std::sync::Mutex::new(veles_core::store::Store::open(
            &home.join("state.db"),
        )?))
    } else {
        None
    };

    let mut reports = Vec::new();
    for step in steps {
        let r = match *step {
            "skills" => veles_core::hermes_migrate::migrate_skills(dry_run)?,
            "cron" => veles_core::hermes_migrate::migrate_cron(
                store.as_ref().expect("store opened for cron"),
                dry_run,
            )?,
            "usage" => veles_core::hermes_migrate::migrate_usage(
                store.as_ref().expect("store opened for usage"),
                None,
                dry_run,
            )?,
            _ => unreachable!(),
        };
        reports.push(r);
    }

    let verb = if dry_run { "would move" } else { "moved" };
    let mut problems = 0usize;
    for r in &reports {
        println!("{}:", r.what);
        println!(
            "  {verb} {}, skipped {} already present",
            r.moved, r.skipped
        );
        for n in &r.notes {
            println!("  · {n}");
        }
        for p in &r.problems {
            problems += 1;
            println!("  ! {p}");
        }
        println!();
    }

    if problems > 0 {
        println!("{problems} item(s) need attention — see the ! lines above.");
    }

    // The claim this whole command exists to make good on, checked
    // rather than asserted: is anything still only in hermes?
    let stranded = veles_core::hermes_migrate::skills_only_in_hermes();
    if stranded.is_empty() {
        println!("Every hermes skill now has a veles copy — ~/.hermes/skills is no");
        println!("longer load-bearing.");
    } else {
        println!("Still only in ~/.hermes/skills ({}):", stranded.len());
        for s in stranded.iter().take(10) {
            println!("  {s}");
        }
        if stranded.len() > 10 {
            println!("  … and {} more", stranded.len() - 10);
        }
        println!("Deleting the hermes install would remove these from the prompt index.");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    /// The boot fence has THREE outcomes, and only one of them runs the
    /// boot-gated duties ungated. B3a folded "refused" into the same
    /// `None` as "single node", so a fresh non-arbiter node — which has
    /// not heard of the arbiter until gossip lands — logged "no telegram
    /// or IRC will start" and started telegram (the 2026-09-28
    /// two-gateway check caught it with a real binary).
    #[test]
    fn a_pool_member_whose_fence_cannot_arm_starts_no_boot_gated_duty() {
        let _g = env_lock();
        let home = tempfile::tempdir().expect("home");
        let prev = std::env::var_os("VELES_HOME");
        std::env::set_var("VELES_HOME", home.path());
        let store = Arc::new(std::sync::Mutex::new(
            veles_core::store::Store::open(&home.path().join("state.db")).expect("store"),
        ));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let alone = Arc::new(veles_core::config::VelesConfig::default());
        let f = arm_singleton_fence(&alone, &store);
        assert!(matches!(f, Fence::Ungated), "a single node runs its duties");
        assert!(matches!(
            rt.block_on(may_start_singleton(&f, "k", "me", "telegram adapter")),
            SingletonStart::Ungated
        ));

        let mut member = veles_core::config::VelesConfig::default();
        member.cluster.gossip_peers = vec!["a".into()];
        member.cluster.arbiter = Some("aaaaaaaa".into());
        let f = arm_singleton_fence(&Arc::new(member), &store);
        assert!(
            matches!(f, Fence::Refused),
            "a pool member that cannot resolve its arbiter is REFUSED, not ungated"
        );
        assert!(matches!(
            rt.block_on(may_start_singleton(&f, "k", "me", "telegram adapter")),
            SingletonStart::No
        ));

        let mut no_arbiter = veles_core::config::VelesConfig::default();
        no_arbiter.cluster.memsync_peers = vec!["a".into()];
        let f = arm_singleton_fence(&Arc::new(no_arbiter), &store);
        assert!(
            matches!(f, Fence::Refused),
            "peers but no arbiter is the double-poller"
        );

        match prev {
            Some(v) => std::env::set_var("VELES_HOME", v),
            None => std::env::remove_var("VELES_HOME"),
        }
    }

    /// A process plugin's lease goes through the SAME fence as the
    /// linked-in adapters: a refused fence starts nothing and says why,
    /// a single node runs the duty ungated.
    #[test]
    fn a_process_plugin_obeys_the_singleton_fence() {
        use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
        use veles_core::pluginhost::LeaseGate;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let runs = Arc::new(AtomicU32::new(0));
        let duty = |runs: Arc<AtomicU32>| -> veles_core::pluginhost::Duty {
            Box::new(move || {
                let runs = runs.clone();
                Box::pin(async move {
                    runs.fetch_add(1, SeqCst);
                    Ok(())
                })
            })
        };

        let refused = FenceGate {
            fence: Fence::Refused,
            me: "me".into(),
        };
        let e = rt
            .block_on(refused.gated("singleton:echo:a", "plugin echo:a", duty(runs.clone())))
            .expect_err("a refused fence must not run the duty");
        assert!(format!("{e:#}").contains("fence"), "{e:#}");
        assert_eq!(runs.load(SeqCst), 0);

        let alone = FenceGate {
            fence: Fence::Ungated,
            me: "me".into(),
        };
        rt.block_on(alone.gated("singleton:echo:a", "plugin echo:a", duty(runs.clone())))
            .unwrap();
        assert_eq!(runs.load(SeqCst), 1);
    }

    use super::*;

    /// `veles model` is `/defaults model`: the same resolution, the same
    /// two keys, and the fallback chain and provider entries untouched
    /// (the old line editor's bug was rewriting the first `model =` in
    /// the file, which was the fallback chain's).
    #[test]
    fn veles_model_writes_the_default_through_the_defaults_resolver() {
        let _env = crate::env_lock();
        let home = tempfile::tempdir().expect("home");
        let prev = std::env::var_os("VELES_HOME");
        std::env::set_var("VELES_HOME", home.path());
        let path = veles_core::config::config_dir().join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"[model]
provider = "custom:ali-token-plan-personal"
default = "deepseek-v4-flash-0731"

[[fallback_providers]]
provider = "custom:zai-coding-plan"
model = "glm-5.3"

[[custom_providers]]
name = "ali-token-plan-personal"
base_url = "http://127.0.0.1:0/v1"
model = "deepseek-v4-flash-0731"

[[custom_providers]]
name = "other"
base_url = "http://127.0.0.1:2/v1"
models = ["m2"]
"#,
        )
        .unwrap();
        run_model(&[
            "m2".to_string(),
            "--provider".to_string(),
            "other".to_string(),
        ])
        .unwrap();
        let cfg: crate::veles_core::config::VelesConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.model.default.as_deref(), Some("m2"));
        assert!(
            cfg.model
                .provider
                .as_deref()
                .is_some_and(|p| p.ends_with("other")),
            "{:?}",
            cfg.model.provider
        );
        assert_eq!(
            cfg.fallback_providers[0].model.as_deref(),
            Some("glm-5.3"),
            "the chain is untouched"
        );
        assert_eq!(
            cfg.custom_providers[0].model.as_deref(),
            Some("deepseek-v4-flash-0731"),
            "a provider entry is untouched"
        );
        let refused = run_model(&["no-such-model@nowhere".to_string()]);
        match prev {
            Some(v) => std::env::set_var("VELES_HOME", v),
            None => std::env::remove_var("VELES_HOME"),
        }
        assert!(
            refused.is_err(),
            "an unresolvable route is an error exit, not a printed shrug"
        );
    }

    /// The desktop cannot read the gateway's `.env` itself — it is a Tcl
    /// process pointed at a URL — so `veles gui` is the hand-off, and the
    /// precedence is the part worth pinning: an exported `VELES_TOKEN` is
    /// how a desktop aimed at a REMOTE gateway carries that gateway's
    /// credential, and silently replacing it with the local one would
    /// point the window at a server that refuses it.
    #[test]
    fn the_launcher_hands_the_desktop_the_right_token() {
        let _guard = crate::env_lock();
        let tmp = tempfile::tempdir().expect("scratch home");
        let dir = tmp.path().to_path_buf();
        std::fs::write(dir.join(".env"), "DESKTOP_BEARER_TOKEN=from-the-dotenv\n").unwrap();
        let prev_home = std::env::var("VELES_HOME").ok();
        let prev_token = std::env::var("VELES_TOKEN").ok();
        let prev_desktop = std::env::var("DESKTOP_BEARER_TOKEN").ok();
        std::env::set_var("VELES_HOME", &dir);
        std::env::remove_var("VELES_TOKEN");
        std::env::remove_var("DESKTOP_BEARER_TOKEN");

        assert_eq!(
            desktop_token_for_child(),
            "from-the-dotenv",
            "the local gateway's own token is the default hand-off"
        );
        std::env::set_var("VELES_TOKEN", "explicit-wins");
        assert_eq!(
            desktop_token_for_child(),
            "explicit-wins",
            "a remote gateway's credential must not be replaced by the local one"
        );
        std::env::set_var("VELES_TOKEN", "");
        assert_eq!(
            desktop_token_for_child(),
            "from-the-dotenv",
            "an EMPTY export is not a credential — `api.tcl` sends no header for one"
        );

        // NOTE: the first branch pins the precedence chain, and an
        // ambient DESKTOP_BEARER_TOKEN would occupy the same layer the
        // chain falls back to (`env_or_dotenv` reads the ENV before the
        // fixture's `.env` line) — observed 2026-09-16 as a gate run in a
        // gateway-adjacent shell, where the live gateway's minted token
        // won the first assertion. Scrubbed above and restored below;
        // the same env-race rule as the module's other env tests.

        match prev_home {
            Some(v) => std::env::set_var("VELES_HOME", v),
            None => std::env::remove_var("VELES_HOME"),
        }
        match prev_token {
            Some(v) => std::env::set_var("VELES_TOKEN", v),
            None => std::env::remove_var("VELES_TOKEN"),
        }
        match prev_desktop {
            Some(v) => std::env::set_var("DESKTOP_BEARER_TOKEN", v),
            None => std::env::remove_var("DESKTOP_BEARER_TOKEN"),
        }
    }

    /// The launcher has to FIND the app before it can hand it a token,
    /// and the chain that finds it rotted once already: the XDG migration
    /// moved `veles_home()` from `~/.veles` to `~/.config/veles`, and the
    /// fallback built from its parent kept looking for
    /// `~/.config/veles-agent/veles-desktop`, which has never existed.
    ///
    /// Pinned against fake layouts rather than the tree the test happens
    /// to run in — `current_exe` under `cargo test` is
    /// `target/debug/deps/…`, four levels under a real checkout, so an
    /// assertion written against the live tree passes here and says
    /// nothing anywhere else.
    #[test]
    fn the_desktop_app_is_found_in_the_layouts_that_exist() {
        let root = std::env::temp_dir().join(format!("veles-gui-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let app = |dir: &std::path::Path| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("main.tcl"), "# the app\n").unwrap();
        };

        // A cargo build: <repo>/target/debug/veles with <repo>/veles-desktop.
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("target/debug")).unwrap();
        app(&repo.join("veles-desktop"));
        assert_eq!(
            desktop_dir_from(Some(repo.join("target/debug")), None).as_deref(),
            Some(repo.join("veles-desktop").as_path()),
            "a cargo debug build sits two levels under the checkout — this is the \
             deployment on this box, and the case that was broken"
        );
        // A nested target triple, one level deeper again.
        std::fs::create_dir_all(repo.join("target/x86_64-unknown-linux-gnu/debug")).unwrap();
        assert_eq!(
            desktop_dir_from(
                Some(repo.join("target/x86_64-unknown-linux-gnu/debug")),
                None
            )
            .as_deref(),
            Some(repo.join("veles-desktop").as_path())
        );

        // An installed layout: $out/bin/veles with $out/veles-desktop.
        let out = root.join("out");
        std::fs::create_dir_all(out.join("bin")).unwrap();
        app(&out.join("veles-desktop"));
        assert_eq!(
            desktop_dir_from(Some(out.join("bin")), None).as_deref(),
            Some(out.join("veles-desktop").as_path()),
            "the nix wrapper sets VELES_DESKTOP_DIR, but a bare $out/bin/veles must work too"
        );

        // Nothing beside the binary ⇒ a checkout in the home directory,
        // which is what the broken arithmetic meant to reach.
        let home = root.join("home");
        app(&home.join("veles-agent/veles-desktop"));
        let nowhere = root.join("nowhere/deep");
        std::fs::create_dir_all(&nowhere).unwrap();
        assert_eq!(
            desktop_dir_from(Some(nowhere.clone()), Some(home.clone())).as_deref(),
            Some(home.join("veles-agent/veles-desktop").as_path()),
            "the home checkout is the last candidate"
        );
        // And the candidate NEAREST the binary wins over the home one, so
        // a checkout being developed is not shadowed by another in $HOME.
        assert_eq!(
            desktop_dir_from(Some(repo.join("target/debug")), Some(home.clone())).as_deref(),
            Some(repo.join("veles-desktop").as_path())
        );

        assert_eq!(
            desktop_dir_from(Some(nowhere), Some(root.join("empty-home"))),
            None,
            "when there is nothing to find, say so — a wrong guess here launches \
             someone else's app"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod flag_wiring_tests {
    /// USER (2026-09-02): "we need the visibility of all of the backend
    /// stuff in all frontends".
    ///
    /// Every `Command::Chat` arm in main.rs must BIND the model /
    /// provider / reasoning flags, not sweep them into `..`. Three arms
    /// swept them — the two interactive ones and the bare-`veles` one —
    /// so `veles chat --plain -m glm-5.3` silently ran the config
    /// default and every surface that reports the model reported the
    /// wrong one. A new arm that forgets them goes red here.
    #[test]
    fn every_interactive_chat_arm_binds_the_model_flags() {
        let src = include_str!("main.rs");
        let mut offenders = Vec::new();
        // Each arm starts at `Some(Command::Chat {` and runs to the
        // closing `}) =>` / `}) if` of its pattern.
        for (i, _) in src.match_indices("Some(Command::Chat {") {
            let rest = &src[i..];
            let end = rest.find("}) =>").or_else(|| rest.find("}) if"));
            let Some(end) = end else { continue };
            let arm = &rest[..end];
            // A one-shot arm (`query: Some(q)`) already binds them; the
            // interactive ones are the ones that used to drop them.
            if !arm.contains("query: None") {
                continue;
            }
            // Only DISPATCH arms — the ones that start a surface. The
            // log-routing match above matches the same shapes and has
            // no business knowing about a model.
            let body_end = (end + 600).min(rest.len());
            if !rest[end..body_end].contains("frontends::") {
                continue;
            }
            let line = src[..i].lines().count() + 1;
            for flag in ["model", "provider", "reasoning"] {
                if !arm.contains(&format!("{flag},")) {
                    offenders.push(format!("main.rs:{line} does not bind `{flag}`"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a chat arm accepts a flag and drops it:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// `--plain` (and VELES_PLAIN=1) is a11y linear mode: it must force
    /// the readline REPL. The dispatch used to re-derive "is this a
    /// terminal?" for itself and never consult either, so on a real TTY
    /// `--plain` started the ratatui TUI — while the log routing, which
    /// DID consult it, sent tracing output to stdout to scribble over
    /// the layout. Both decisions come from `tui_bound` now.
    #[test]
    fn the_surface_choice_and_the_log_routing_agree() {
        let src = include_str!("main.rs");
        let dispatch = src
            .split_once("let tui_bound = match &cli.command")
            .expect("tui_bound is computed in main")
            .1;
        // Past the tui_bound match itself: the dispatch is what follows.
        let dispatch = dispatch
            .split_once("match &cli.command")
            .map(|(_, rest)| rest)
            .unwrap_or(dispatch);
        assert!(
            !dispatch.contains("is_terminal()"),
            "the dispatch re-derives the surface instead of using tui_bound"
        );
        assert!(
            dispatch.contains("if tui_bound"),
            "the dispatch must branch on tui_bound"
        );
    }
}

/// What the first-run wizard writes has to be readable by the build that
/// consumes it.
///
/// Three of the keys it wrote were dead: `api_key_env` (the field is
/// `key_env`), `bot_token_env` (the token is env-only and was never a
/// config key), and the whole `[platforms.ircx]` table with `tls`, `nick`
/// and an array-shaped `channels` inside it. Nothing caught them because
/// the dead-key audit reads the LIVE config, and a wizard's output only
/// becomes live on a machine that ran the wizard — so a fresh install got
/// a provider with no credentials, a telegram platform with no path to its
/// token, and an IRC block that never turned the adapter on.
///
/// Each block is checked two ways: parsed into the real config type, where
/// a key nobody deserializes simply lands nowhere, and — for the two
/// connectors that declare a config surface — every key it emits is one
/// the connector says it reads.
///
/// Mutation: `key_env` back to `api_key_env` fails the parse assertion, and
/// `use_tls` back to `tls` fails the surface check.
#[cfg(test)]
mod setup_wizard_tests {
    // The block builders moved to `veles_core::setup` (one writer, so the
    // CLI and the desktop's Setup… window cannot drift — `60786dd`). These
    // tests stay HERE rather than moving with them, because they are the
    // cross-crate half of the shape check: core cannot depend on
    // `veles_irc`/`veles_telegram`, so only the composition root can assert
    // that every key the builder emits is one the LIVE adapter declares it
    // reads (`config_surface()`). The pure "does it deserialize / is the
    // model pair a table / are secrets out of config" half lives in
    // `veles_core::setup::tests`.
    //
    // `veles_core` reaches this crate THROUGH `veles_runtime` (the lib's
    // `use veles_runtime::veles_core;` is in scope here), not as a direct
    // dependency — under the shared-core dylib model (M1) veles-cli links
    // only the runtime, which re-exports core.
    use crate::veles_core::setup::{irc_setup_block, provider_setup_block, telegram_setup_block};

    /// The `key = ` lines a block emits, without their values. Comments and
    /// table headers are not keys — a commented example is documentation,
    /// and the wizard's own `# channels = …` line is one.
    fn emitted_keys(block: &str) -> Vec<String> {
        block
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('['))
            .filter_map(|l| l.split_once('='))
            .map(|(k, _)| k.trim().to_string())
            .collect()
    }

    /// Every key the block emits, checked against what the connector
    /// declares it reads.
    fn assert_declared(block: &str, table: &str, declared: &[String]) {
        for key in emitted_keys(block) {
            let path = format!("{table}.{key}");
            assert!(
                declared.contains(&path),
                "{path} is not a key the connector reads:\n{block}\ndeclared: {declared:?}"
            );
        }
    }

    #[test]
    fn the_provider_block_names_the_field_the_resolver_reads() {
        let block = provider_setup_block(
            "local",
            "http://localhost:8000/v1",
            "OPENAI_API_KEY",
            "chat_completions",
            "",
        );
        assert!(
            !block.contains("driver"),
            "no driver for a native wire: {block}"
        );
        let cfg: crate::veles_core::config::VelesConfig = toml::from_str(&block).expect("parses");
        assert_eq!(cfg.custom_providers.len(), 1, "{block}");
        let p = &cfg.custom_providers[0];
        assert_eq!(p.name, "local");
        assert_eq!(p.base_url, "http://localhost:8000/v1");
        assert_eq!(p.api_mode, "chat_completions");
        assert_eq!(
            p.key_env.as_deref(),
            Some("OPENAI_API_KEY"),
            "a key nothing deserializes is a provider with no credentials: {block}"
        );
    }

    #[test]
    fn the_irc_block_configures_the_adapter_that_reads_it() {
        let block = irc_setup_block("irc.libera.chat");
        let declared: Vec<String> = veles_irc::config_surface()
            .iter()
            .map(|k| k.path.clone())
            .collect();
        assert_declared(&block, "platforms.irc", &declared);

        let cfg: crate::veles_core::config::VelesConfig = toml::from_str(&block).expect("parses");
        let plat = cfg
            .platforms
            .get("irc")
            .expect("the table `irc_enabled` and `apply_mapping` look up");
        assert_eq!(plat.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            plat.get("server").and_then(|v| v.as_str()),
            Some("irc.libera.chat")
        );
        assert_eq!(plat.get("port").and_then(|v| v.as_u64()), Some(6697));
        assert_eq!(
            plat.get("use_tls").and_then(|v| v.as_bool()),
            Some(true),
            "`tls` is not the key the adapter maps: {block}"
        );
        assert_eq!(
            plat.get("nickname").and_then(|v| v.as_str()),
            Some("veles"),
            "`nick` is not the key the adapter maps: {block}"
        );
    }

    #[test]
    fn the_telegram_block_holds_a_switch_and_no_secret() {
        let block = telegram_setup_block();
        let declared: Vec<String> = veles_telegram::config_surface()
            .iter()
            .map(|k| k.path.clone())
            .collect();
        assert_declared(&block, "platforms.telegram", &declared);
        assert!(
            !block.to_lowercase().contains("token"),
            "the token is env-only; a config key for it is a secret in a file people \
             copy around:\n{block}"
        );

        let cfg: crate::veles_core::config::VelesConfig = toml::from_str(&block).expect("parses");
        let plat = cfg.platforms.get("telegram").expect("the platform table");
        assert_eq!(plat.get("enabled").and_then(|v| v.as_bool()), Some(true));
    }
}

// The fake editors here are shell snippets (`printf 'hello\n' >`,
// `sed -i …`, `true`): `platform::editor_command` runs an editor through
// `sh -c "<editor> \"$1\""` on unix, and that shape is what these tests
// drive. On Windows an editor is a program (`notepad`), the snippets are
// "program not found", and the three went red on the first Windows test
// run (2026-10-13). What they prove — the revision log, the empty save,
// the concurrent write — is not platform code.
#[cfg(all(test, unix))]
mod memory_edit_tests {
    use super::*;
    use veles_core::memory::MemoryStore;

    /// A store in its own directory, with the config lookups pinned there
    /// too — `MemoryStore::at` loads config, and the real one would pull
    /// the developer's `.env` into this process.
    fn scratch() -> (tempfile::TempDir, MemoryStore, veles_core::config::DirGuard) {
        let dir = tempfile::tempdir().unwrap();
        let g =
            veles_core::config::set_dirs(Some(veles_core::config::DirOverride::forced(dir.path())));
        let s = MemoryStore::at(dir.path().to_path_buf());
        (dir, s, g)
    }

    fn revisions(s: &MemoryStore, path: &str) -> usize {
        s.log_rows(Some(path), 1000).unwrap().len()
    }

    #[test]
    fn edit_creates_changes_and_skips_an_unchanged_file() {
        let (_d, s, _g) = scratch();
        run_memory_edit(&s, "reference/e.md", "printf 'hello\\n' >").unwrap();
        assert_eq!(s.read_file("reference/e.md").unwrap(), "hello\n");
        run_memory_edit(&s, "reference/e.md", "sed -i s/hello/bye/").unwrap();
        assert_eq!(s.read_file("reference/e.md").unwrap(), "bye\n");
        assert_eq!(revisions(&s, "reference/e.md"), 2);
        run_memory_edit(&s, "reference/e.md", "true").unwrap();
        assert_eq!(revisions(&s, "reference/e.md"), 2, "no change, no revision");
        let log = s.log_rows(Some("reference/e.md"), 1).unwrap();
        assert_eq!(log[0].source, "edit");
    }

    /// Saving an empty file is not a delete by accident.
    #[test]
    fn an_edit_saved_empty_records_nothing() {
        let (_d, s, _g) = scratch();
        s.write_file("reference/e.md", "keep me").unwrap();
        let err = run_memory_edit(&s, "reference/e.md", "printf '' >")
            .unwrap_err()
            .to_string();
        assert!(err.contains("veles memory rm"), "{err}");
        assert_eq!(s.read_file("reference/e.md").unwrap(), "keep me");
        // The temp file is kept on purpose (it is the user's text); a
        // test must not leave one behind in $XDG_RUNTIME_DIR every run.
        let kept = err.rsplit("the temp file is ").next().unwrap().trim();
        assert!(std::fs::remove_file(kept).is_ok(), "kept file: {kept}");
    }

    /// Independent edits started in the same clock tick must still use
    /// different files. Keep this about recovery files, not two saves to
    /// one memory path: that second case is a REQUIRED conflict, not a
    /// permitted race. The old timestamp-only name failed in this shape.
    #[test]
    fn independent_edits_have_private_distinct_recovery_files() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, s, _g) = scratch();
        s.write_file("reference/e.md", "keep me").unwrap();
        let barrier = std::sync::Barrier::new(3);
        let kept = std::thread::scope(|scope| {
            let jobs: Vec<_> = [0, 1]
                .into_iter()
                .map(|_| {
                    let s = &s;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        // The editor fails, so neither thread attempts a
                        // conflicting save; only their recovery paths are
                        // under test here.
                        let err = run_memory_edit(s, "reference/e.md", "false")
                            .unwrap_err()
                            .to_string();
                        let path = err.rsplit("your text is in ").next().unwrap().trim();
                        let path = std::path::PathBuf::from(path);
                        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
                        #[cfg(unix)]
                        assert_eq!(
                            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                            0o600
                        );
                        path
                    })
                })
                .collect();
            barrier.wait();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_ne!(kept[0], kept[1], "a frozen clock must not alias two edits");
        for path in kept {
            std::fs::remove_file(path).unwrap();
        }
    }

    /// Two saves from the same revision cannot both land. The winner's
    /// content survives; the stale editor is refused and its text stays
    /// in its own recovery file. Distinct temp names must not weaken
    /// the optimistic-concurrency guard.
    #[test]
    fn concurrent_saves_keep_the_winner_and_preserve_the_conflicted_text() {
        let (_dir, s, _g) = scratch();
        s.write_file("reference/e.md", "base\n").unwrap();
        let barrier = std::sync::Barrier::new(3);
        let results: Vec<Result<(), String>> = std::thread::scope(|scope| {
            let jobs: Vec<_> = ["first", "second"]
                .into_iter()
                .map(|text| {
                    let s = &s;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        run_memory_edit(s, "reference/e.md", &format!("sed -i s/base/{text}/"))
                            .map_err(|e| format!("{e:#}"))
                    })
                })
                .collect();
            barrier.wait();
            jobs.into_iter().map(|job| job.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let conflict = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("the stale save must be refused");
        assert!(conflict.contains("changed since it was read"), "{conflict}");
        let recovery = conflict.rsplit("your text is in ").next().unwrap().trim();
        let saved = s.read_file("reference/e.md").unwrap();
        let loser = if saved == "first\n" {
            "second\n"
        } else {
            "first\n"
        };
        assert!(saved == "first\n" || saved == "second\n", "{saved:?}");
        assert_eq!(std::fs::read_to_string(recovery).unwrap(), loser);
        std::fs::remove_file(recovery).unwrap();
    }

    /// The agent writes the file while it is open in the editor: the
    /// edit must not overwrite that, and must not lose the typed text.
    #[test]
    fn a_write_during_the_edit_is_not_overwritten_and_the_text_is_kept() {
        let (dir, s, _g) = scratch();
        s.write_file("reference/e.md", "v1").unwrap();
        let agent = MemoryStore::at(dir.path().to_path_buf());
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            agent.write_file("reference/e.md", "agent v2").unwrap();
        });
        let err = run_memory_edit(&s, "reference/e.md", "sleep 1.5; printf 'mine' >")
            .unwrap_err()
            .to_string();
        writer.join().unwrap();
        assert!(err.contains("nothing was overwritten"), "{err}");
        assert_eq!(s.read_file("reference/e.md").unwrap(), "agent v2");
        let kept = err.rsplit("your text is in ").next().unwrap().trim();
        assert_eq!(std::fs::read_to_string(kept).unwrap(), "mine");
        let _ = std::fs::remove_file(kept);
    }

    #[test]
    fn revisions_parse_with_or_without_the_r() {
        assert_eq!(parse_rev("r12").unwrap(), 12);
        assert_eq!(parse_rev("12").unwrap(), 12);
        assert!(parse_rev("twelve").is_err());
    }
}
