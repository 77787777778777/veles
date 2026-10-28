//! Gateway lifecycle: pid file, shutdown records, crash-loop guard.
//!
//! Makes the gateway observable and operable:
//!   - `~/.local/share/veles/gateway.pid` — who owns the gateway (status/stop/restart
//!     resolve it without a systemd dependency).
//!   - `~/.local/share/veles/gateway-shutdown.log` — one line per START/STOP with a
//!     reason, so `veles gateway status` and the next boot can tell a
//!     clean stop from a crash, and so restarts are visible.
//!   - Crash-loop guard: N abnormal exits inside a window → loud warning
//!     on the next boot (systemd's StartLimitBurst is the actual brake;
//!     this is the explainer).

use anyhow::Context as _;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::veles_home;

// The current thread's owner tag (M3/M5). Set by the host when it
// builds an instance's runtime threads; `None` on every core thread.
// The owner-aware panic hook reads this to decide whether a panic is a
// MODULE fault (mark faulted, let the runtime unwind) or a CORE fault
// (abort, as today). The tag is a thread-scoped string like
// "module:echo"; the OWNER is the prefix before the first ':'.
// (Not a doc comment: rustdoc cannot document a macro invocation.)
thread_local! {
    static THREAD_OWNER: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Tag the current thread with an owner. The host calls this from its
/// instance runtime's `on_thread_start`. Leaks the tag string on
/// purpose: it must outlive the thread.
pub fn set_thread_owner(tag: &'static str) {
    THREAD_OWNER.with(|o| o.set(Some(tag)));
}

/// The current thread's owner tag, if any.
pub fn thread_owner() -> Option<&'static str> {
    THREAD_OWNER.with(|o| o.get())
}

/// Run `f` with the current thread tagged `tag`, the previous tag back
/// after — for an owner that borrows a core thread rather than owning
/// one: the Tcl loader sourcing a plugin (`tcl:<name>`, R7), so what the
/// script registers while being sourced is attributed to it.
pub fn with_thread_owner<R>(tag: &'static str, f: impl FnOnce() -> R) -> R {
    let prev = THREAD_OWNER.with(|o| o.replace(Some(tag)));
    let r = f();
    THREAD_OWNER.with(|o| o.set(prev));
    r
}

/// The `&'static str` for an owner tag, leaked ONCE per distinct tag
/// (a tag must outlive every thread and registry entry carrying it, and
/// a Tcl plugin is loaded many times over a gateway's life).
pub fn owner_tag(tag: &str) -> &'static str {
    static INTERNED: std::sync::OnceLock<std::sync::Mutex<Vec<&'static str>>> =
        std::sync::OnceLock::new();
    let mut g = INTERNED
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(t) = g.iter().find(|t| **t == tag) {
        return t;
    }
    let leaked: &'static str = Box::leak(tag.to_string().into_boxed_str());
    g.push(leaked);
    leaked
}

/// Restore a previous owner tag (test-only). Production sets the tag once
/// from a runtime's thread-start hook and never clears it; a TEST scopes a
/// tag so one case's module identity does not leak into the next case on
/// the shared test runner thread. `prev` is what `thread_owner()` returned
/// before the test set its tag.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_thread_owner_for_test(prev: Option<&'static str>) {
    THREAD_OWNER.with(|o| o.set(prev));
}

/// Make a panic on ANY thread terminate the process — unless the thread
/// is a MODULE thread and the panic is located OUTSIDE core's crates
/// (M5). In that case the hook marks the instance faulted and lets the
/// unwinding reach the runtime, whose supervisor tears the instance
/// down and restarts it with backoff; the process lives.
///
/// Rust's default is to unwind the panicking thread and leave the rest of
/// the process running. For a tokio server that means a panicked task
/// vanishes and the gateway limps on in a broken state — which is
/// exactly what happened on 2026-08-22: one `blocking_lock()` inside an
/// async handler panicked, poisoned the store mutex, and every later
/// request panicked on the poison. The unit has `Restart=on-failure`,
/// but systemd never saw a failure because the process never exited:
/// `NRestarts=0` across the whole incident. It stayed up and broken
/// until someone restarted it by hand.
///
/// So: log the panic with its location, record an abnormal stop for the
/// crash-loop guard, then abort. Combined with poison-recovering locks
/// (see `crate::lockutil`), the split is deliberate — tolerate a
/// poisoned lock left by someone else's panic, but die on the panic
/// itself, so the supervisor can do its job.
///
/// M5 refines the split at the MODULE boundary: a panic on a thread
/// tagged with an owner, located outside `crates/veles-{core,frontends,
/// types}/`, is module code failing, not core failing. Aborting the
/// whole gateway for one module's bug is the failure the whole module
/// architecture exists to prevent.
pub fn install_fatal_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Default hook first: it prints the message and honours
        // RUST_BACKTRACE.
        previous(info);
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();

        // M5: a MODULE thread panicking OUTSIDE core's crates is a
        // module fault. Mark it and let the runtime's unwinding reach
        // the supervisor; do NOT take the process down.
        if let Some(owner) = thread_owner() {
            let in_core = info
                .location()
                .map(|l| {
                    // `Location::file()` carries the build host's
                    // separators: `\` on Windows would miss every
                    // `crates/…` test below and call a core panic on a
                    // module thread a module fault.
                    let f = l.file().replace('\\', "/");
                    f.contains("crates/veles-core/")
                        || f.contains("crates/veles-frontends/")
                        || f.contains("crates/veles-types/")
                })
                .unwrap_or(false);
            if !in_core {
                tracing::error!(
                    "module panic on thread '{name}' (owner {owner}) at {loc} — \
                     marking the instance faulted; the runtime unwinds and the \
                     supervisor restarts it, the gateway lives"
                );
                crate::module::mark_faulted(owner, &loc);
                return;
            }
        }

        tracing::error!(
            "panic on thread '{name}' at {loc} — aborting so the supervisor restarts us"
        );
        record_stop(&format!("panic at {loc} (thread {name})"));
        remove_pid();
        // abort, not exit: unwinding from here can deadlock on the very
        // lock whose holder just panicked.
        std::process::abort();
    }));
}

pub fn pid_file() -> PathBuf {
    veles_home().join("gateway.pid")
}

fn shutdown_log() -> PathBuf {
    veles_home().join("gateway-shutdown.log")
}

fn now_ts() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // RFC3339-ish local time, no deps: epoch + readable offset would need
    // chrono; keep the record machine-parsable with UTC seconds and let
    // `status`/logs format it.
    format!("{secs}")
}

fn append(line: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(shutdown_log())
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Write our pid. Fails loudly if another live gateway owns the file.
pub fn write_pid() -> anyhow::Result<()> {
    let path = pid_file();
    if let Ok(existing) = fs::read_to_string(&path) {
        if let Ok(pid) = existing.trim().parse::<u32>() {
            if blocks_start(pid, std::process::id(), is_running) {
                anyhow::bail!(
                    "gateway already running (pid {pid}, {path:?}) — \
                     stop it first (`veles gateway stop`)"
                );
            }
        }
    }
    // The home may not exist yet — a first `veles gateway run` under a
    // fresh VELES_HOME died here with a bare "No such file or directory
    // (os error 2)" that named nothing (the plan's Step 5.2). Create it,
    // exactly like Store::open creates state.db's parent; and if THAT
    // fails, the error names the path it could not make.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating the veles home {}", parent.display()))?;
    }
    fs::write(&path, format!("{}\n", std::process::id()))
        .with_context(|| format!("writing the pid file {}", path.display()))?;
    Ok(())
}

/// Does the gateway recorded in the pid file stop this one from
/// starting? Not when the pid is OUR OWN. In a container the gateway is
/// always pid 1, so a file left by a gateway that was killed before it
/// could remove it (SIGKILL after the stop timeout) names the new one,
/// which is alive and runs `gateway`: every start on the VPS refused
/// itself as "already running (pid 1)" for six minutes, 2026-10-10.
fn blocks_start(recorded: u32, own: u32, running: impl Fn(u32) -> bool) -> bool {
    recorded != own && running(recorded)
}

/// The SIGTERM and SIGINT listeners the pin made, kept until the
/// gateway's shutdown loop takes them (`shutdown_signals`): a stop that
/// arrives while the plugins start is held in them, where a listener
/// made later would never hear it.
#[cfg(unix)]
static PARKED_SHUTDOWN_SIGNALS: std::sync::Mutex<
    Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
> = std::sync::Mutex::new(None);

/// The gateway's own signal handlers, pinned before any foreign code
/// runs in the process. tokio installs one OS handler per signal at the
/// first listener and keeps it for the process's life, so this settles
/// SIGTERM and SIGINT (the shutdown) and SIGCHLD (the process driver's
/// fallback) once. Why so early: a language runtime's boot saves the
/// dispositions it finds and puts them back when done (lang-racket,
/// about 300 ms), and the listeners used to go in at the end of
/// `GatewayCore::run`, after the modules had started. A boot that
/// straddled that point restored SIG_DFL over the fresh handlers, and
/// tokio never installs a handler twice: the VPS gateway, pid 1 of its
/// container, ignored podman's stop for 35 s and was killed
/// (2026-10-10).
#[cfg(unix)]
pub fn pin_signal_handlers() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut parked = PARKED_SHUTDOWN_SIGNALS
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if parked.is_none() {
        *parked = Some((
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        ));
    }
    // Dropped at once: the OS handler it installed stays for the process's
    // life, which is all SIGCHLD needs here (tokio's process driver makes
    // its own listener when it wants one).
    let _ = signal(SignalKind::child())?;
    Ok(())
}

/// The listeners for the gateway's shutdown signals, SIGTERM and SIGINT:
/// the pinned ones, with any stop that came during the start still in
/// them, or fresh ones when nothing pinned.
#[cfg(unix)]
pub fn shutdown_signals(
) -> std::io::Result<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)> {
    use tokio::signal::unix::{signal, SignalKind};
    let parked = PARKED_SHUTDOWN_SIGNALS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    match parked {
        Some(pair) => Ok(pair),
        None => Ok((
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )),
    }
}

/// The signals this process catches (a handler installed, neither the
/// default nor ignored), read from the kernel's own list: the `SigCgt`
/// mask in /proc/self/status. Empty when that cannot be read.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn caught_signals() -> Vec<i32> {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let Some(mask) = status.lines().find_map(|l| l.strip_prefix("SigCgt:")) else {
        return Vec::new();
    };
    let Ok(bits) = u64::from_str_radix(mask.trim(), 16) else {
        return Vec::new();
    };
    (1..=64).filter(|s| bits & (1u64 << (s - 1)) != 0).collect()
}

pub fn remove_pid() {
    let _ = fs::remove_file(pid_file());
}

pub fn read_pid() -> Option<u32> {
    let path = pid_file();
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Is the gateway that wrote this pid still running?
///
/// Alive is not enough. A pid file outlives its gateway whenever the
/// process is killed before it can remove the file, and the pid is then
/// handed to someone else. A reboot does both: Android sends SIGTERM,
/// kills the process before the graceful shutdown ends, and the next
/// boot gives the same low pids to its own early processes. The
/// Android app's gateway then refused every start after one reboot in
/// two, "already running" about a system daemon (emulator smoke test,
/// 2026-09-30).
pub fn is_running(pid: u32) -> bool {
    crate::platform::pid_alive(pid) && crate::platform::runs_a_gateway(pid)
}

/// Ask the gateway at `pid` to stop gracefully: the IPC `shutdown`
/// request first (every platform; the same wake its signal handler
/// gives), then the platform's stop signal where one exists. `true`
/// when either reached it.
pub fn request_stop(pid: u32) -> bool {
    if let Some(peer) = crate::runsock::peer_for_pid(pid) {
        let req = serde_json::json!({"op": "shutdown"});
        if crate::runsock::request(&peer, &req).is_ok() {
            return true;
        }
    }
    crate::platform::request_stop(pid)
}

/// The in-process stop request (the IPC `shutdown` op, or a frontend's
/// own quit): [`wait_for_stop`] wakes on it. A request before anyone
/// waits is kept, like a signal that arrived while the plugins started.
static STOP: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub fn request_shutdown() {
    STOP.notify_one();
}

/// Resolves when the gateway should stop: SIGTERM/SIGINT (the pinned
/// listeners, with a stop that came during the start still in them) on
/// unix, the console control events on Windows, or [`request_shutdown`]
/// anywhere. Create it once before the serving loop and poll it from
/// the loop's select.
pub async fn wait_for_stop() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let (mut term, mut int) = shutdown_signals()?;
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = STOP.notified() => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
        let mut c = ctrl_c()?;
        let mut b = ctrl_break()?;
        let mut cl = ctrl_close()?;
        let mut lo = ctrl_logoff()?;
        let mut sh = ctrl_shutdown()?;
        tokio::select! {
            _ = c.recv() => {}
            _ = b.recv() => {}
            _ = cl.recv() => {}
            _ = lo.recv() => {}
            _ = sh.recv() => {}
            _ = STOP.notified() => {}
        }
    }
    #[cfg(not(any(unix, windows)))]
    STOP.notified().await;
    Ok(())
}

/// A stop the gateway meant: a clean shutdown, or the idle exit
/// (`--exit-when-idle`, the phone), which every start after it used to
/// call "abnormal (idle)" — and three in ten minutes, a crash loop.
fn meant(reason: &str) -> bool {
    matches!(reason, "clean" | "idle")
}

/// Gateway boot bookkeeping. Returns any warnings from the previous
/// shutdown (abnormal exit, possible crash loop) to log loudly.
pub fn record_start() -> Vec<String> {
    let mut warnings = Vec::new();
    let log = shutdown_log();
    if let Ok(text) = fs::read_to_string(&log) {
        let lines: Vec<&str> = text.lines().collect();
        if let Some(last) = lines.last() {
            if let Some(reason) = last.strip_prefix("STOP ") {
                let reason = reason.split_once(' ').map(|(_, r)| r).unwrap_or(reason);
                if !meant(reason) {
                    warnings.push(format!(
                        "previous gateway exit was abnormal ({reason}) — check the log"
                    ));
                }
            }
        }
        // Crash-loop smell: ≥3 abnormal stops in the last 10 minutes.
        let now = now_ts().parse::<u64>().unwrap_or(0);
        let mut abnormal = 0usize;
        for line in lines {
            if let Some(rest) = line.strip_prefix("STOP ") {
                if let Some((ts, reason)) = rest.split_once(' ') {
                    if !meant(reason) {
                        if let Ok(ts) = ts.parse::<u64>() {
                            if now.saturating_sub(ts) < 600 {
                                abnormal += 1;
                            }
                        }
                    }
                }
            }
        }
        if abnormal >= 3 {
            warnings.push(format!(
                "{abnormal} abnormal gateways exits in the last 10 min — \
                 possible crash loop (systemd: `systemctl --user status veles-gateway`)"
            ));
        }
    }
    append(&format!(
        "START {} v{}",
        now_ts(),
        env!("CARGO_PKG_VERSION")
    ));
    warnings
}

pub fn record_stop(reason: &str) {
    append(&format!("STOP {} {reason}", now_ts()));
}

/// List the most recent lifecycle records (for `veles gateway status`).
pub fn recent_records(n: usize) -> Vec<String> {
    let log = shutdown_log();
    match fs::read_to_string(&log) {
        Ok(text) => text.lines().rev().take(n).map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn uptime_from_records() -> Option<u64> {
    // Find the most recent START without a following STOP → running since.
    let log = fs::read_to_string(shutdown_log()).ok()?;
    let mut start_ts: Option<u64> = None;
    for line in log.lines() {
        if let Some(rest) = line.strip_prefix("START ") {
            start_ts = rest.split_once(' ').and_then(|(ts, _)| ts.parse().ok());
        } else if let Some(_rest) = line.strip_prefix("STOP ") {
            start_ts = None;
        }
    }
    let start = start_ts?;
    let now = now_ts().parse::<u64>().ok()?;
    Some(now.saturating_sub(start))
}

pub fn has_systemd_unit() -> bool {
    let unit = std::path::Path::new(
        &std::env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| std::env::var("HOME").unwrap_or_else(|_| ".".into())),
    )
    .join("systemd/user/veles-gateway.service");
    unit.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hook must ABORT, not unwind.
    ///
    /// This is the whole point of it: on 2026-08-22 a panicked tokio task
    /// left the gateway up and broken, and `Restart=on-failure` never
    /// fired because the process never exited (`NRestarts=0`). Asserting
    /// on the signal is the only way to know the hook still does that —
    /// a hook that merely logs would look identical from inside.
    ///
    /// Re-runs this very test in a child process, because a passing
    /// abort would take the test runner with it.
    #[cfg(unix)]
    #[test]
    fn fatal_panic_hook_aborts_the_process() {
        use std::os::unix::process::ExitStatusExt;

        const CHILD: &str = "VELES_TEST_PANIC_HOOK_CHILD";

        if std::env::var(CHILD).is_ok() {
            // Child leg: arm the hook and panic. Must not return.
            // NOTE: allocate nothing that owns a cleanup — this leg
            // SIGABRTs, so any `tempfile::TempDir` created above the
            // branch would skip its Drop and leak a `.tmp*` dir on
            // every run. The child writes its breadcrumb under the
            // VELES_HOME the parent passes in the environment.
            //
            // Not dumpable: the abort is the point, its core is not. With
            // systemd-coredump every suite run stored one (~450 KB each,
            // 284 MB on the SSD by 2026-10-04); a non-dumpable process
            // never reaches core_pattern at all.
            #[cfg(target_os = "linux")]
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
            }
            install_fatal_panic_hook();
            panic!("deliberate panic to prove the hook aborts");
        }

        let home = tempfile::tempdir().expect("tempdir");

        let exe = std::env::current_exe().expect("test binary path");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "lifecycle::tests::fatal_panic_hook_aborts_the_process",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("VELES_HOME", home.path())
            .output()
            .expect("spawn child test");

        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "panic must abort the process (got status {:?}); \
             without this systemd's Restart=on-failure never fires.\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );

        // And it must leave a breadcrumb for the crash-loop guard.
        let log = home.path().join("gateway-shutdown.log");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            text.contains("panic at"),
            "abnormal stop must be recorded for the crash-loop guard, got: {text:?}"
        );
    }

    /// The phone stops itself when idle, again and again: no start after
    /// that is an abnormal exit, and no run of them a crash loop. A real
    /// error still is.
    #[test]
    fn an_idle_stop_is_not_abnormal() {
        let (_home, _env) = crate::testenv::scratch_home();
        for _ in 0..3 {
            record_start();
            record_stop("idle");
        }
        assert_eq!(record_start(), Vec::<String>::new());
        record_stop("clean");
        assert_eq!(record_start(), Vec::<String>::new());
        record_stop("error: boom");
        let w = record_start();
        assert!(
            w.iter().any(|l| l.contains("abnormal (error: boom)")),
            "{w:?}"
        );
    }

    /// A child killed on drop, so a failing assert does not leave it.
    #[cfg(unix)]
    struct Child(std::process::Child);
    #[cfg(unix)]
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spawn, then wait until /proc shows the child's own argv. std
    /// spawns with vfork, and the parent runs again before the kernel
    /// has set the new argv: read at once, cmdline is empty, which made
    /// the live gateway below look like no gateway, and would make the
    /// reused pid's "not a gateway" true for the wrong reason.
    #[cfg(unix)]
    fn spawn(cmd: &str, args: &[&str]) -> Child {
        let child = Child(
            std::process::Command::new(cmd)
                .args(args)
                .spawn()
                .expect("spawn"),
        );
        let path = format!("/proc/{}/cmdline", child.0.id());
        for _ in 0..200 {
            if fs::read(&path).is_ok_and(|argv| argv.starts_with(cmd.as_bytes())) {
                return child;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{cmd}'s argv never showed in {path}");
    }

    /// A live process that is not a gateway does not own the pid file.
    /// On Android a reboot hands the dead gateway's pid to a system
    /// daemon, and "already running" then refused every start.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a_reused_pid_is_not_a_running_gateway() {
        let other = spawn("sleep", &["30"]);
        assert!(
            !is_running(other.0.id()),
            "`sleep 30` is alive but no gateway; a stale pid file naming it must not block a start"
        );

        let (_home, _env) = crate::testenv::scratch_home();
        fs::write(pid_file(), format!("{}\n", other.0.id())).unwrap();
        write_pid().expect("a stale pid file must not block the gateway");
        assert_eq!(read_pid(), Some(std::process::id()));
    }

    /// And a live gateway still does.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a_live_gateway_is_running() {
        // `sh -c CMD NAME`: NAME is the shell's $0, and it stays in the
        // shell's argv because the `;` keeps sh from exec'ing sleep.
        let gateway = spawn("sh", &["-c", "sleep 30; :", "gateway"]);
        assert!(is_running(gateway.0.id()));

        let (_home, _env) = crate::testenv::scratch_home();
        fs::write(pid_file(), format!("{}\n", gateway.0.id())).unwrap();
        let err = write_pid().expect_err("a live gateway owns the pid file");
        assert!(err.to_string().contains("already running"), "{err}");
    }

    /// A pid file naming our own pid is ours: in a container the
    /// gateway is always pid 1, and the file a killed one left behind
    /// names its successor. Another live gateway still blocks.
    #[test]
    fn our_own_pid_in_the_file_does_not_block_the_start() {
        let alive = |_| true;
        assert!(!blocks_start(1, 1, alive), "pid 1 in a container is us");
        assert!(blocks_start(7, 1, alive), "another live gateway blocks");
        assert!(!blocks_start(7, 1, |_| false), "a dead one does not");

        let (_home, _env) = crate::testenv::scratch_home();
        fs::write(pid_file(), format!("{}\n", std::process::id())).unwrap();
        write_pid().expect("our own pid in the file must not block us");
        assert_eq!(read_pid(), Some(std::process::id()));
    }

    /// A home that does not exist yet is CREATED, not refused: a first
    /// `veles gateway run` under a fresh VELES_HOME used to die with a
    /// bare "No such file or directory (os error 2)" naming nothing
    /// (Step 5.2 of the phone plan). The override is nested over
    /// scratch_home's, thread-scoped, so no env var moves.
    #[test]
    fn a_missing_home_is_created_for_the_pid_file() {
        let (tmp, _env) = crate::testenv::scratch_home();
        let deep = tmp.path().join("not").join("yet");
        let _dirs = crate::config::set_dirs(Some(crate::config::DirOverride::forced(deep.clone())));
        assert!(!deep.exists(), "the premise: the home is missing");

        write_pid().expect("a missing home must not refuse the gateway");

        assert!(deep.is_dir(), "the home was created");
        assert_eq!(read_pid(), Some(std::process::id()));
    }

    /// And when the home CANNOT be created — a plain file sits where the
    /// directory would go — the error names the path instead of the bare
    /// errno. That message is the whole fix: it is what tells the
    /// operator which home is wrong.
    #[test]
    fn an_uncreatable_home_names_its_path() {
        let (tmp, _env) = crate::testenv::scratch_home();
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let deep = blocker.join("home");
        let _dirs = crate::config::set_dirs(Some(crate::config::DirOverride::forced(deep.clone())));

        let err = write_pid().expect_err("a file is not a home");
        let text = format!("{err:#}");
        assert!(
            text.contains("creating the veles home"),
            "the error says what it was doing: {text}"
        );
        assert!(
            text.contains(&deep.display().to_string()),
            "…and names the path it could not make: {text}"
        );
    }

    /// The pin is what the kernel sees (/proc/self/status lists SIGTERM,
    /// SIGINT and SIGCHLD as caught), and a stop that comes before the
    /// gateway's loop listens is kept for it: the loop takes the pinned
    /// listeners, so the SIGTERM raised here is still in them.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pinned_signal_handlers_are_caught_and_keep_an_early_stop() {
        pin_signal_handlers().expect("pinned");
        let caught = caught_signals();
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD] {
            assert!(
                caught.contains(&sig),
                "signal {sig} is not caught: {caught:?}"
            );
        }
        // A stop while the plugins start. The sleep lets the signal
        // driver hand it out before anyone takes the listeners, as a
        // real start does; a listener made after that never sees it.
        // SAFETY: raising a signal this process catches (checked above).
        unsafe { libc::raise(libc::SIGTERM) };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let (mut term, _int) = shutdown_signals().expect("listeners");
        let got = tokio::time::timeout(std::time::Duration::from_secs(3), term.recv()).await;
        assert!(
            got.is_ok(),
            "the SIGTERM raised before the loop listened was lost"
        );
    }
}
