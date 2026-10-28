//! One unix socket per veles process — how processes that share a
//! `VELES_HOME` find each other (PLAN-sessions-and-phone.md S2, USER
//! decision D1).
//!
//! Live sessions are spread over processes: the gateway (its chats and
//! the desktop's sessions), every `veles chat` TUI or REPL, and their
//! delegate children. `session_runs` only knows which of them is running
//! a turn RIGHT NOW; an idle TUI sitting at its prompt is in no table at
//! all. So each process that holds sessions binds
//! `<data dir>/run/<pid>.sock` and answers four one-line JSON requests:
//!
//! - `{"op":"hello"}` → `{"pid", "surface"}`
//! - `{"op":"open_sessions"}` → `{"sessions":[{"session_id","surface"}…]}`
//!   (`crate::presence`)
//! - `{"op":"nudge","session":sid}` → `{"held":bool}`: a mail is waiting
//!   in that session's queue; the process that holds it runs it now if
//!   it is idle (its turn boundary drains it otherwise).
//! - `{"op":"mail_done","mail":id}` → `{"ok":true}`: the turn a mail
//!   started has ended; wake whoever here waits for its reply.
//!
//! Presence is never written to SQLite: a heartbeat writer per process
//! is exactly the kind of steady write traffic the busy handler starves
//! on. "Who holds session X" is answered by asking each socket, and a
//! socket whose process is gone is unlinked by whoever finds it.
//!
//! Trust: the directory is 0700 and, on Linux and Android, the peer's
//! uid must be ours (`SO_PEERCRED`); on Windows the pipe's DACL admits
//! this user only. A process running as the veles user can already read
//! `state.db` and the provider keys, so the socket grants nothing the
//! filesystem does not; it is owner-class, like the desktop token.
//!
//! The transport is the platform layer's (`platform::ipc_*`): a unix
//! socket at the marker path, or a named pipe the marker file names.
//! Everything above it — the marker directory, the pid sweep, the
//! one-line protocol — is this file and the same everywhere.
//!
//! A fifth request, `{"op":"shutdown"}` → `{"ok":true}`, asks a gateway
//! to stop the way SIGTERM does; `veles gateway stop` sends it first on
//! every platform (Windows has no signal to send).

use crate::platform::{self, IpcListener, IpcStream};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// `sockaddr_un.sun_path` is 108 bytes on Linux (104 on the BSDs),
/// including the terminating NUL. Paths longer than this cannot be
/// bound at all, so a deep `VELES_HOME` falls back to a short directory
/// derived from it. (Windows' marker files have no such limit; the
/// rule is kept there too so every platform agrees on the directory.)
const SUN_PATH_MAX: usize = 100;

/// One request's whole budget, read and write.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// A request line larger than this is refused (ops are tiny).
const MAX_LINE: usize = 64 * 1024;

/// The socket directory for this data dir. Deterministic, so every
/// process sharing the data dir agrees on it.
pub fn run_dir() -> PathBuf {
    run_dir_for(&crate::config::veles_home())
}

/// [`run_dir`] for an explicit data dir.
pub fn run_dir_for(home: &Path) -> PathBuf {
    let natural = home.join("run");
    // The longest name we bind is the pid (≤ 10 digits) + ".sock".
    if natural.as_os_str().len() + 1 + 16 <= SUN_PATH_MAX {
        return natural;
    }
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(home.as_os_str().as_encoded_bytes());
    let tag: String = h.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("veles-{}-{tag}", platform::user_tag()))
}

fn sock_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.sock"))
}

fn ensure_dir(dir: &Path) -> Result<()> {
    platform::private_dir(dir).with_context(|| format!("create {}", dir.display()))
}

/// What this process serves under, once bound.
struct Serving {
    path: PathBuf,
    surface: String,
}

/// The runtime request threads run under. A nudge spawns work
/// (`rt::spawn`, the desktop's drain loop), and a bare std thread has no
/// runtime context — `tokio::spawn` would panic there. The runtime that
/// called [`serve`] when there was one (the gateway's), else the detached
/// background runtime (`crate::bg`).
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

static SERVING: Mutex<Option<Serving>> = Mutex::new(None);

/// Bind this process's socket and answer on a background thread.
/// Idempotent: a second call keeps the first binding (and its surface
/// name). Best-effort by design — a process that cannot bind still
/// works; it just cannot be nudged or listed by its siblings, which is
/// what every process was before this module. Returns the path bound.
pub fn serve(surface: &str) -> Option<PathBuf> {
    let mut guard = SERVING.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = guard.as_ref() {
        return Some(s.path.clone());
    }
    let dir = run_dir();
    if let Err(e) = ensure_dir(&dir) {
        tracing::debug!("runsock: no run dir ({e:#}); this process is not reachable");
        return None;
    }
    sweep_dead(&dir);
    let path = sock_path(&dir, std::process::id());
    // A file at our own pid's path belongs to a dead process that had
    // the same pid; ours is not bound yet.
    let listener = match platform::ipc_bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::debug!("runsock: bind {} failed: {e}", path.display());
            return None;
        }
    };
    let _ =
        RUNTIME.set(tokio::runtime::Handle::try_current().unwrap_or_else(|_| crate::bg::handle()));
    *guard = Some(Serving {
        path: path.clone(),
        surface: surface.to_string(),
    });
    drop(guard);
    let spawned = std::thread::Builder::new()
        .name("veles-runsock".into())
        .spawn(move || accept_loop(listener));
    if spawned.is_err() {
        shutdown();
        return None;
    }
    Some(path)
}

/// Unlink this process's socket (orderly exits). A process that dies
/// without calling it leaves a file the next [`peers`] sweep removes.
pub fn shutdown() {
    let mut guard = SERVING.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = guard.take() {
        platform::ipc_unbind(&s.path);
    }
}

/// The surface this process serves as, when it serves.
pub fn serving_surface() -> Option<String> {
    SERVING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|s| s.surface.clone())
}

fn accept_loop(mut listener: IpcListener) {
    loop {
        let stream = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                // A transient refusal must not spin a core; a dead
                // listener keeps this thread parked, as a closed unix
                // listener would.
                tracing::debug!("runsock: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        // One short-lived thread per request: a nudge may take the
        // store lock, and a slow peer must not hold up the next one.
        let _ = std::thread::Builder::new()
            .name("veles-runsock-req".into())
            .spawn(move || handle(stream));
    }
}

fn handle(mut stream: IpcStream) {
    if !stream.peer_same_user() {
        return;
    }
    let _rt = RUNTIME.get().map(|h| h.enter());
    let _ = stream.set_timeout(IO_TIMEOUT);
    let req = match read_line(&stream) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(stream, "{}", json!({"error": format!("{e:#}")}));
            return;
        }
    };
    let resp = dispatch(&req);
    let _ = writeln!(stream, "{resp}");
    let _ = stream.flush();
}

/// One JSON line, read through a cap of [`MAX_LINE`] so a peer cannot
/// make us buffer an unbounded line.
fn read_line(stream: &IpcStream) -> Result<Value> {
    let mut reader = BufReader::new(std::io::Read::take(stream, MAX_LINE as u64));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(anyhow!("empty line"));
    }
    Ok(serde_json::from_str(line.trim())?)
}

/// Answer one request. Public for tests and for in-process callers that
/// want the same answer a sibling would get.
pub fn dispatch(req: &Value) -> Value {
    match req.get("op").and_then(Value::as_str) {
        Some("hello") => json!({
            "pid": std::process::id(),
            "surface": serving_surface().unwrap_or_default(),
        }),
        Some("open_sessions") => {
            let sessions: Vec<Value> = crate::presence::list()
                .into_iter()
                .map(|s| json!({"session_id": s.session_id, "surface": s.surface}))
                .collect();
            json!({"pid": std::process::id(), "sessions": sessions})
        }
        Some("nudge") => match req.get("session").and_then(Value::as_str) {
            Some(sid) => json!({"held": crate::presence::nudge(sid)}),
            None => json!({"error": "nudge needs a session"}),
        },
        Some("mail_done") => match req.get("mail").and_then(Value::as_str) {
            Some(id) => {
                crate::mail::notify_done(id);
                json!({"ok": true})
            }
            None => json!({"error": "mail_done needs a mail id"}),
        },
        // The graceful stop a signal would be: the gateway's shutdown
        // select wakes on it (`lifecycle::wait_for_stop`). Only a peer
        // of our own user reaches this line.
        Some("shutdown") => {
            crate::lifecycle::request_shutdown();
            json!({"ok": true, "pid": std::process::id()})
        }
        Some(other) => json!({"error": format!("unknown op {other:?}")}),
        None => json!({"error": "no op"}),
    }
}

/// A sibling process's socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub pid: u32,
    pub path: PathBuf,
}

/// Every OTHER live process's socket in this data dir. Files whose
/// process is gone are unlinked on the way.
pub fn peers() -> Vec<Peer> {
    peers_in(&run_dir())
}

fn peers_in(dir: &Path) -> Vec<Peer> {
    let me = std::process::id();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        let Some(pid) = sock_pid(&path) else { continue };
        if pid == me {
            continue;
        }
        if !platform::pid_alive(pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        out.push(Peer { pid, path });
    }
    out.sort_by_key(|p| p.pid);
    out
}

fn sock_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(".sock")?.parse().ok()
}

fn sweep_dead(dir: &Path) {
    let _ = peers_in(dir);
}

/// The socket a given pid would serve, when it is live.
pub fn peer_for_pid(pid: u32) -> Option<Peer> {
    if pid == std::process::id() {
        return None;
    }
    let path = sock_path(&run_dir(), pid);
    (path.exists() && platform::pid_alive(pid)).then_some(Peer { pid, path })
}

/// One request to one sibling: connect, write a line, read a line.
pub fn request(peer: &Peer, req: &Value) -> Result<Value> {
    let mut s = platform::ipc_connect(&peer.path)
        .with_context(|| format!("connect {}", peer.path.display()))?;
    s.set_timeout(IO_TIMEOUT)?;
    writeln!(s, "{req}")?;
    s.flush()?;
    let v = read_line(&s)?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Err(anyhow!("{} answered: {e}", peer.pid));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deep_home_falls_back_to_a_short_dir_and_a_shallow_one_does_not() {
        let shallow = Path::new("/home/u/.local/share/veles");
        assert_eq!(run_dir_for(shallow), shallow.join("run"));
        let deep = PathBuf::from(format!("/{}", "d".repeat(150)));
        let d = run_dir_for(&deep);
        assert!(d.as_os_str().len() + 17 <= SUN_PATH_MAX, "{}", d.display());
        // Deterministic: every process sharing the home agrees.
        assert_eq!(d, run_dir_for(&deep));
        // And distinct per home.
        let other = PathBuf::from(format!("/{}", "e".repeat(150)));
        assert_ne!(d, run_dir_for(&other));
    }

    #[test]
    fn a_dead_pids_socket_is_swept_and_a_live_one_is_kept() {
        let dir = crate::testenv::kept_dir()
            .join(format!("runsock-sweep-{}", uuid::Uuid::new_v4().simple()));
        ensure_dir(&dir).unwrap();
        // Our own parent is alive on every platform (a test runner);
        // u32::MAX - 1 is never a real pid.
        let live_pid = live_other_pid();
        let live = sock_path(&dir, live_pid);
        let dead = sock_path(&dir, u32::MAX - 1);
        std::fs::write(&live, b"").unwrap();
        std::fs::write(&dead, b"").unwrap();
        let ps = peers_in(&dir);
        assert_eq!(ps.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![live_pid]);
        assert!(live.exists());
        assert!(!dead.exists(), "the dead pid's socket file is unlinked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pid that is alive and not ours: init on unix, the System
    /// process (pid 4) on Windows.
    fn live_other_pid() -> u32 {
        if cfg!(windows) {
            4
        } else {
            1
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_run_dir_is_private_even_when_it_existed_wider() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testenv::kept_dir()
            .join(format!("runsock-mode-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_round_trip_over_a_real_socket_answers_hello_and_refuses_junk() {
        let dir = crate::testenv::kept_dir()
            .join(format!("runsock-rt-{}", uuid::Uuid::new_v4().simple()));
        ensure_dir(&dir).unwrap();
        let path = dir.join("7.sock");
        let mut listener = platform::ipc_bind(&path).unwrap();
        std::thread::spawn(move || {
            for _ in 0..2 {
                handle(listener.accept().unwrap());
            }
        });
        let peer = Peer {
            pid: 0,
            path: path.clone(),
        };
        let v = request(&peer, &json!({"op": "hello"})).unwrap();
        assert_eq!(v["pid"], json!(std::process::id()));
        let e = request(&peer, &json!({"op": "nope"})).unwrap_err();
        assert!(format!("{e:#}").contains("unknown op"), "{e:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
