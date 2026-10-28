//! The engine: ECL booted ONCE per process on one OS thread of its own,
//! and a mailbox between Lisp and the rest of the gateway.
//!
//! ECL's threads are real OS threads (Boehm-collected), so the loop in
//! `runtime.lisp` simply blocks in `VELES-SYS:%NEXT` until the mailbox
//! has something, and handlers run in threads of their own: a slow one
//! blocks nothing, one past its deadline is killed (`mp:process-kill`),
//! and an unload kills the script's threads and deletes its packages. A
//! host call a handler makes (`register_tool`, `http_get`, …) is a
//! request with an id; the Rust side runs it on the instance runtime and
//! pushes the reply into the mailbox. Every string crosses as ASCII JSON
//! (non-ASCII as `\u` escapes) in a base string, so no encoding step
//! touches either side.
//!
//! ECL is process-global and makes process-wide choices at boot unless
//! told not to; each guard below was a crash or a hang in the spike
//! (2026-10-10): its signal handlers (SEGV/BUS/ILL/FPE/INT/PIPE) are off,
//! so a fault on a Rust thread stays Rust's; its atexit `cl_shutdown`
//! runs on the non-Lisp main thread and `pthread_exit`s it, so the
//! process never exits, and is disarmed; its threads' C stack checks
//! assume RLIMIT_STACK while the threads get glibc's default stack, so a
//! deep recursion segfaulted, and the default is raised to 32 MB; Boehm
//! starts nproc-1 marker threads, capped at 2.
//!
//! The engine outlives the module instance: `stop` unloads the scripts
//! and leaves ECL booted (it cannot be booted twice), the engine thread
//! keeps the module mapped (the module host reports it `kept mapped`, by
//! design), and the next `start` of the same mapping reuses it with the
//! new instance's runtime. A SECOND copy of this module finds core's
//! boot mark taken and refuses to start: replacing the Lisp host is a
//! gateway restart.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use veles_runtime::tokio;
use veles_runtime::tokio::sync::{oneshot, watch};
use veles_runtime::veles_core;
use veles_runtime::veles_core::scripthost::{BoxFut, ScriptCx, ScriptHost, ScriptRef};

use crate::ffi::{self, Obj, NIL};

/// How long a `load` may take: the file's reading plus whatever its top
/// level does (each registration is a host call and back).
const LOAD_DEADLINE: Duration = Duration::from_secs(90);
/// The boot handshake: cl_boot (about 40 ms) and the two Lisp files.
const BOOT_DEADLINE: Duration = Duration::from_secs(30);
const UNLOAD_DEADLINE: Duration = Duration::from_secs(10);
/// The engine thread's stack, and the default for every thread created
/// with default attributes from the boot on (ECL's own among them). ECL
/// assumes RLIMIT_STACK (8 MB here and in the VPS container) for each of
/// its threads; the real stack must be larger, or its overflow check
/// fires past the guard page (measured: a segfault at ~8.4 MB).
const THREAD_STACK: usize = 32 << 20;
/// The Lisp heap's ceiling (Boehm): past it, an allocation signals a
/// storage condition in the script instead of growing the gateway.
const HEAP_LIMIT: isize = 512 << 20;

/// Why an `ask` has no answer: Lisp said no (`Replied`: the Lisp side ran
/// its own cleanup before answering, a failed load's package deletion
/// among it), or nothing came back at all (`NoAnswer`: the deadline
/// passed, or the engine is dead), when whatever Lisp would have done is
/// this side's to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    Replied(String),
    NoAnswer(String),
}

impl AskError {
    fn into_string(self) -> String {
        match self {
            AskError::Replied(s) | AskError::NoAnswer(s) => s,
        }
    }
}

/// The seconds the Lisp side lets a handler run before killing it; the
/// seam's own deadlines (300/60/30 s) are what a caller sees first.
fn invoke_deadline_s() -> u64 {
    std::env::var("VELES_LISP_INVOKE_DEADLINE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Where ECL is: `VELES_ECL_HOME` at run time, else the install the crate
/// was built against.
pub fn ecl_home() -> PathBuf {
    std::env::var("VELES_ECL_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("VELES_ECL_HOME")))
}

/// The install's library dir (`lib/ecl-<version>/`): ECLDIR, where
/// `require` finds the contrib modules.
fn ecl_dir(home: &Path) -> Option<PathBuf> {
    std::fs::read_dir(home.join("lib"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("ecl-"))
        })
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineState {
    Booting,
    /// Up, with ECL's version.
    Up(String),
    Dead(String),
}

pub struct LispEngine {
    mailbox: Mutex<VecDeque<String>>,
    /// Signalled on every push: the loop waits on it in `%NEXT`.
    arrived: Condvar,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<String, AskError>>>>,
    /// The scripts the seam handed this host, by name: their `cx` is
    /// what a host call from Lisp code goes through.
    scripts: Mutex<BTreeMap<String, Arc<ScriptCx>>>,
    /// The CURRENT instance's runtime: host calls are spawned onto it.
    handle: Mutex<Option<tokio::runtime::Handle>>,
    next_id: AtomicU64,
    state: watch::Sender<EngineState>,
    /// What the boot form reported before it returned (an error that
    /// escaped the runtime): the engine thread's death reason.
    last_words: Mutex<Option<String>>,
    started: Instant,
    home: PathBuf,
    /// Where `runtime.lisp` and `veles.lisp` are written for the boot.
    lisp_dir: PathBuf,
}

/// The one engine of this process (this mapping of the module).
static ENGINE: OnceLock<Arc<LispEngine>> = OnceLock::new();

impl LispEngine {
    /// The engine of this process: booted on the first call (the caller
    /// waits for the handshake), reused by every later one with the new
    /// instance's runtime handle. `state_dir` is the module's state dir.
    pub async fn get_or_boot(
        handle: tokio::runtime::Handle,
        state_dir: &Path,
    ) -> Result<Arc<LispEngine>, String> {
        Self::get_or_boot_with(handle, state_dir, crate::RUNTIME_LISP).await
    }

    /// `get_or_boot` with the runtime's Lisp source given, for a test that
    /// boots a broken one (in its own process: ECL boots once).
    pub async fn get_or_boot_with(
        handle: tokio::runtime::Handle,
        state_dir: &Path,
        runtime_lisp: &str,
    ) -> Result<Arc<LispEngine>, String> {
        if let Some(e) = ENGINE.get() {
            *e.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
            return match e.state() {
                EngineState::Dead(why) => Err(format!("the Lisp engine died earlier: {why}")),
                _ => Ok(Arc::clone(e)),
            };
        }
        if !veles_core::scripthost::mark_engine_booted(crate::KIND) {
            return Err(
                "lang-lisp is pinned: ECL boots once per process and another copy of this \
                 module booted it; replacing the Lisp host is a gateway restart"
                    .into(),
            );
        }
        let home = ecl_home();
        if !home.join("lib/libecl.so").exists() || ecl_dir(&home).is_none() {
            return Err(format!(
                "VELES_ECL_HOME={}: no lib/libecl.so or lib/ecl-<version>/ (an ECL install is expected)",
                home.display()
            ));
        }
        let lisp_dir = state_dir.join("lisp");
        write_sources(&lisp_dir, runtime_lisp)?;
        let (state, _) = watch::channel(EngineState::Booting);
        let engine = Arc::new(LispEngine {
            mailbox: Mutex::new(VecDeque::new()),
            arrived: Condvar::new(),
            waiting: Mutex::new(HashMap::new()),
            scripts: Mutex::new(BTreeMap::new()),
            handle: Mutex::new(Some(handle.clone())),
            next_id: AtomicU64::new(0),
            state,
            last_words: Mutex::new(None),
            started: Instant::now(),
            home,
            lisp_dir,
        });
        let e = ENGINE.get_or_init(|| Arc::clone(&engine));
        let me = Arc::clone(e);
        // The engine runs on a thread of its own (its stack is ECL's to
        // size), joined by one of the instance runtime's blocking threads
        // on purpose: the module host counts that one, so the teardown of
        // this instance reports the module kept mapped (which it is: the
        // engine thread runs this module's code for the process's life).
        drop(handle.spawn_blocking(move || {
            let engine = Arc::clone(&me);
            let spawned = std::thread::Builder::new()
                .name("veles-lisp".into())
                .stack_size(THREAD_STACK)
                .spawn(move || engine_thread(engine));
            match spawned {
                Ok(t) => {
                    let _ = t.join();
                }
                Err(err) => {
                    let _ = me.state.send(EngineState::Dead(format!(
                        "the engine thread did not start: {err}"
                    )));
                }
            }
        }));
        let mut rx = e.state.subscribe();
        let handshake = tokio::time::timeout(BOOT_DEADLINE, async {
            loop {
                match rx.borrow_and_update().clone() {
                    EngineState::Booting => {}
                    EngineState::Up(v) => return Ok(v),
                    EngineState::Dead(why) => return Err(why),
                }
                if rx.changed().await.is_err() {
                    return Err("the engine thread ended before the handshake".to_string());
                }
            }
        })
        .await;
        match handshake {
            Ok(Ok(_)) => Ok(Arc::clone(e)),
            Ok(Err(why)) => Err(format!("ECL failed to boot: {why}")),
            Err(_) => Err(format!(
                "ECL did not answer the boot handshake within {BOOT_DEADLINE:?}"
            )),
        }
    }

    pub fn state(&self) -> EngineState {
        self.state.borrow().clone()
    }

    /// The instance is stopping: forget its runtime, so a host call
    /// arriving before the next `start` is refused rather than spawned
    /// onto a runtime that is shutting down (where it would never run).
    pub fn detach(&self) {
        *self.handle.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// ECL's version once up.
    pub fn version(&self) -> String {
        match self.state() {
            EngineState::Up(v) => v,
            other => format!("{other:?}"),
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// The scripts loaded right now, by name.
    pub fn loaded(&self) -> Vec<String> {
        self.scripts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Unload every script (the module's stop); ECL stays booted.
    pub async fn unload_all(&self) -> usize {
        let names = self.loaded();
        for name in &names {
            let _ = ScriptHost::unload(self, name).await;
        }
        names.len()
    }

    /// Into the mailbox, and the loop woken.
    fn push(&self, msg: Value) {
        self.mailbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(ascii_json(&msg));
        self.arrived.notify_one();
    }

    /// A request with an id and a slot for its reply.
    fn request(&self, mut msg: Value) -> (u64, oneshot::Receiver<Result<String, AskError>>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        msg["id"] = json!(id);
        if let EngineState::Dead(why) = self.state() {
            let _ = tx.send(Err(AskError::NoAnswer(format!(
                "the Lisp engine is dead: {why}"
            ))));
            return (id, rx);
        }
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        self.push(msg);
        (id, rx)
    }

    async fn ask(&self, msg: Value, deadline: Duration) -> Result<String, AskError> {
        let (id, rx) = self.request(msg);
        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(AskError::NoAnswer(
                "the Lisp engine dropped the request".into(),
            )),
            Err(_) => {
                self.waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(AskError::NoAnswer(format!(
                    "no answer from Lisp within {deadline:?}"
                )))
            }
        }
    }

    /// Everything Lisp sends (`%SEND`), on whichever Lisp thread sent it.
    fn on_message(&self, text: &str) {
        let m: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                veles_core::module::log(
                    veles_runtime::tracing::Level::WARN,
                    &format!("lang-lisp: unreadable message from Lisp ({e}): {text}"),
                );
                return;
            }
        };
        match m["t"].as_str().unwrap_or("") {
            "booted" => {
                let v = m["version"].as_str().unwrap_or("?").to_string();
                let _ = self.state.send(EngineState::Up(v));
            }
            "reply" => {
                let id = m["id"].as_u64().unwrap_or(0);
                let slot = self
                    .waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                if let Some(tx) = slot {
                    let r = match m["err"].as_str() {
                        Some(e) => Err(AskError::Replied(e.to_string())),
                        None => Ok(m["ok"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| m["ok"].to_string())),
                    };
                    let _ = tx.send(r);
                }
            }
            "host" => self.on_host_call(&m),
            "log" => {
                let name = m["name"].as_str().unwrap_or("");
                let level = match m["level"].as_str().unwrap_or("info") {
                    "error" => veles_runtime::tracing::Level::ERROR,
                    "warn" | "warning" => veles_runtime::tracing::Level::WARN,
                    "debug" => veles_runtime::tracing::Level::DEBUG,
                    "trace" => veles_runtime::tracing::Level::TRACE,
                    _ => veles_runtime::tracing::Level::INFO,
                };
                let text = m["text"].as_str().unwrap_or("");
                let owner = self
                    .scripts
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(name)
                    .map(|cx| cx.owner);
                let line = if name.is_empty() {
                    format!("lang-lisp: {text}")
                } else {
                    format!("lisp:{name}: {text}")
                };
                match owner {
                    Some(o) => veles_core::lifecycle::with_thread_owner(o, || {
                        veles_core::module::log(level, &line)
                    }),
                    None => veles_core::module::log(level, &line),
                }
            }
            other => veles_core::module::log(
                veles_runtime::tracing::Level::WARN,
                &format!("lang-lisp: unknown message kind `{other}` from Lisp"),
            ),
        }
    }

    /// A host call from a script's handler: run it on the instance
    /// runtime, answer through the mailbox.
    fn on_host_call(&self, m: &Value) {
        let id = m["id"].as_u64().unwrap_or(0);
        let name = m["name"].as_str().unwrap_or("").to_string();
        let op = m["op"].as_str().unwrap_or("").to_string();
        let args: Vec<String> = m["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cx = self
            .scripts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&name)
            .cloned();
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let reply = move |r: Result<String, String>| match r {
            Ok(v) => json!({"t": "host-reply", "id": id, "ok": v}),
            Err(e) => json!({"t": "host-reply", "id": id, "err": e}),
        };
        match (cx, handle, ENGINE.get()) {
            (Some(cx), Some(h), Some(e)) => {
                let e = Arc::clone(e);
                drop(h.spawn(async move {
                    let r = cx.call(&op, args).await;
                    e.push(reply(r));
                }));
            }
            (None, _, _) => self.push(reply(Err(format!(
                "lisp:{name}: not loaded (a host call after its unload?)"
            )))),
            _ => self.push(reply(Err(
                "the Lisp host has no instance runtime right now".into(),
            ))),
        }
    }
}

impl ScriptHost for LispEngine {
    fn kind(&self) -> &'static str {
        crate::KIND
    }

    fn load(&self, script: ScriptRef) -> BoxFut<'_, Result<String, String>> {
        Box::pin(async move {
            self.scripts
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(script.name.clone(), Arc::clone(&script.cx));
            let r = self
                .ask(
                    json!({
                        "t": "load",
                        "name": script.name,
                        "entry": script.entry.display().to_string(),
                        "dir": script.dir.display().to_string(),
                    }),
                    LOAD_DEADLINE,
                )
                .await;
            match &r {
                Ok(_) => {}
                Err(AskError::Replied(_)) => {
                    // The load raised (or tried to exit): the Lisp side
                    // killed the script's threads and deleted its packages
                    // before it answered; only the host-call door closes here.
                    self.scripts
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&script.name);
                }
                Err(AskError::NoAnswer(_)) => {
                    // The deadline passed with the load's thread still
                    // running (or the engine is gone): the unload is what
                    // kills it, best effort.
                    self.scripts
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&script.name);
                    let _ = self
                        .ask(json!({"t": "unload", "name": script.name}), UNLOAD_DEADLINE)
                        .await;
                }
            }
            r.map_err(AskError::into_string)
        })
    }

    fn unload(&self, name: &str) -> BoxFut<'_, Result<String, String>> {
        let name = name.to_string();
        Box::pin(async move {
            self.scripts
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&name);
            self.ask(json!({"t": "unload", "name": name}), UNLOAD_DEADLINE)
                .await
                .map_err(AskError::into_string)
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
            self.ask(
                json!({"t": "invoke", "name": name, "cb": callback, "payload": payload}),
                Duration::from_secs(invoke_deadline_s() + 5),
            )
            .await
            .map_err(AskError::into_string)
        })
    }
}

/// JSON text with every character past 127 as a `\u` escape (a UTF-16
/// pair past the BMP): the wire is ASCII, so a Lisp base string holds it
/// as it is. Non-ASCII only ever appears inside JSON strings, so the
/// replacement is safe anywhere in the text.
pub fn ascii_json(v: &Value) -> String {
    let text = v.to_string();
    if text.is_ascii() {
        return text;
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// The bytes of a Lisp base string (`%SEND`'s and `%DEAD`'s argument),
/// or None for anything else. The Lisp side always passes a simple base
/// string; anything else is refused here rather than signalled from C.
///
/// # Safety
/// Called on a Lisp thread with an object Lisp passed in.
unsafe fn base_string(o: Obj) -> Option<String> {
    if ffi::si_base_string_p(o) == NIL {
        return None;
    }
    let p = ffi::ecl_base_string_pointer_safe(o);
    if p.is_null() {
        return None;
    }
    Some(CStr::from_ptr(p).to_string_lossy().into_owned())
}

/// Lisp → Rust: `(veles-sys:%next timeout-ms)`, the next mailbox message
/// as a base string, or NIL after the timeout. Blocks this Lisp thread
/// (the loop's) on the mailbox; the Lisp side holds interrupts off around
/// it.
///
/// # Safety
/// A `cl_objectfn_fixed` of one argument; ECL calls it on a Lisp thread.
pub unsafe extern "C" fn veles_lisp_next(timeout: Obj) -> Obj {
    let Some(e) = ENGINE.get() else {
        return NIL;
    };
    let ms = ffi::fixnum_value(timeout).unwrap_or(200).clamp(0, 60_000) as u64;
    let next = {
        let mut q = e.mailbox.lock().unwrap_or_else(|p| p.into_inner());
        if q.is_empty() {
            q = e
                .arrived
                .wait_timeout(q, Duration::from_millis(ms))
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
        q.pop_front()
    };
    match next {
        Some(s) => ffi::ecl_make_simple_base_string(
            s.as_ptr() as *const std::os::raw::c_char,
            s.len() as isize,
        ),
        None => NIL,
    }
}

/// Lisp → Rust: `(veles-sys:%send text)`, a message (a reply, a host
/// call, a log line, the boot handshake).
///
/// # Safety
/// A `cl_objectfn_fixed` of one argument; ECL calls it on a Lisp thread.
pub unsafe extern "C" fn veles_lisp_send(msg: Obj) -> Obj {
    if let (Some(e), Some(text)) = (ENGINE.get(), base_string(msg)) {
        e.on_message(&text);
    }
    NIL
}

/// Lisp → Rust: `(veles-sys:%dead text)`, the boot form's last words (an
/// error that escaped the runtime before the loop ran, or out of it).
///
/// # Safety
/// A `cl_objectfn_fixed` of one argument; ECL calls it on a Lisp thread.
pub unsafe extern "C" fn veles_lisp_dead(msg: Obj) -> Obj {
    if let (Some(e), Some(text)) = (ENGINE.get(), base_string(msg)) {
        *e.last_words.lock().unwrap_or_else(|p| p.into_inner()) = Some(text);
    }
    NIL
}

/// The whole life of the engine thread: boot, the loop, and — should the
/// loop ever return — the death notice.
fn engine_thread(engine: Arc<LispEngine>) {
    let t0 = Instant::now();
    let why = match boot_and_run(&engine) {
        // The boot form returned: it reports the error it caught first (a
        // runtime that did not even load), and that is why.
        Ok(()) => engine
            .last_words
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| "the Lisp loop returned".to_string()),
        Err(e) => e,
    };
    let _ = engine.state.send(EngineState::Dead(why.clone()));
    let waiting: Vec<_> = engine
        .waiting
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain()
        .map(|(_, tx)| tx)
        .collect();
    for tx in waiting {
        let _ = tx.send(Err(AskError::NoAnswer(format!(
            "the Lisp engine died: {why}"
        ))));
    }
    veles_core::module::log(
        veles_runtime::tracing::Level::ERROR,
        &format!(
            "lang-lisp: the engine thread ended after {:.0?}: {why}",
            t0.elapsed()
        ),
    );
}

/// The ECL options the boot sets, each with what it keeps from the
/// gateway: its signal handlers, its exit, its stdin.
const BOOT_OPTIONS: [(i32, isize); 7] = [
    // A fault on a Rust thread stays Rust's (with ECL's handler it ended
    // just that thread, silently). Lisp's own stack overflows are checked
    // by ECL without a signal.
    (ffi::opt::TRAP_SIGSEGV, 0),
    (ffi::opt::TRAP_SIGBUS, 0),
    (ffi::opt::TRAP_SIGILL, 0),
    // No float traps either: `(/ 1.0 0.0)` is infinity, and Rust code that
    // runs on a Lisp thread (the natives) never meets a trap it does not
    // expect.
    (ffi::opt::TRAP_SIGFPE, 0),
    // The gateway's own: its shutdown signal and its ignored SIGPIPE.
    (ffi::opt::TRAP_SIGINT, 0),
    (ffi::opt::TRAP_SIGPIPE, 0),
    // No thread of ECL's sigwait()s the process's asynchronous signals.
    (ffi::opt::SIGNAL_HANDLING_THREAD, 0),
];

/// Disarm ECL's own atexit `cl_shutdown`, registered by `cl_boot`: at
/// exit it runs on the main thread, which is no Lisp thread, so it fails
/// and `pthread_exit`s that thread, and the process never exits (the
/// spike's main thread a zombie with 18 live threads). Registered after
/// the boot, so it runs first.
extern "C" fn disarm_ecl_shutdown() {
    // SAFETY: ECL_OPT_BOOTED is past the options ECL refuses to change
    // once booted; -1 makes cl_shutdown skip its work.
    unsafe { ffi::ecl_set_option(ffi::opt::BOOTED, -1) };
}

/// Read and evaluate `src` with `si:safe-eval`: the value, or an error
/// naming the form when evaluating it signalled.
fn eval(src: &str) -> Result<Obj, String> {
    let c = CString::new(src).map_err(|e| e.to_string())?;
    // SAFETY: ECL is booted on this thread; every object lives only for
    // this call, on this thread's stack (Boehm scans it).
    unsafe {
        let s = ffi::ecl_make_simple_base_string(c.as_ptr(), src.len() as isize);
        let form = ffi::si_string_to_object(1, s);
        let failed = ffi::ecl_make_symbol(c"EVAL-FAILED".as_ptr(), c"KEYWORD".as_ptr());
        let r = ffi::si_safe_eval(3, form, NIL, failed);
        if r == failed {
            Err(format!("the boot form failed: {:.120}", src))
        } else {
            Ok(r)
        }
    }
}

/// A Lisp string literal of `s` (a path, ASCII by construction).
fn lisp_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Boot ECL on this thread, then load the runtime and run its loop,
/// which never returns.
fn boot_and_run(engine: &LispEngine) -> Result<(), String> {
    let ecl_dir = ecl_dir(&engine.home).ok_or("no lib/ecl-<version>/ in VELES_ECL_HOME")?;
    // libecl names the package it was built in as its library dir; the
    // trimmed runtime rewrote that away, so ECLDIR says where it is.
    std::env::set_var("ECLDIR", format!("{}/", ecl_dir.display()));
    if std::env::var_os("GC_MARKERS").is_none() {
        std::env::set_var("GC_MARKERS", "2");
    }
    // SAFETY: plain attribute calls on a local; the default they set is
    // the stack of every thread created without one from here on (ECL's).
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        libc::pthread_attr_init(&mut attr);
        libc::pthread_attr_setstacksize(&mut attr, THREAD_STACK);
        let rc = ffi::pthread_setattr_default_np(&attr);
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 {
            return Err(format!("pthread_setattr_default_np: error {rc}"));
        }
    }
    // The gateway's signal handling is the process's, not ECL's: the
    // options above keep its hands off, and whatever it installs anyway
    // is put back. Right only because the gateway's handlers are already
    // in: the module host pins them before any module's code runs.
    let before = signal_dispositions();
    for (option, value) in BOOT_OPTIONS {
        // SAFETY: options before the boot, as ECL requires.
        unsafe { ffi::ecl_set_option(option, value) };
    }
    // SAFETY: as above.
    unsafe { ffi::ecl_set_option(ffi::opt::HEAP_SIZE, HEAP_LIMIT) };
    let arg0 = CString::new("veles").map_err(|e| e.to_string())?;
    let mut argv = [arg0.into_raw(), std::ptr::null_mut()];
    // SAFETY: called once per process (core's boot mark), on the thread
    // that stays ECL's for the process's life; argv lives for ever.
    let rc = unsafe { ffi::cl_boot(1, argv.as_mut_ptr()) };
    restore_signal_dispositions(&before);
    if rc == 0 {
        return Err("cl_boot failed".into());
    }
    // SAFETY: registering a plain function with the C library.
    unsafe { libc::atexit(disarm_ecl_shutdown) };
    eval(r#"(progn (defpackage "VELES-SYS" (:use) (:export "%NEXT" "%SEND" "%DEAD")) t)"#)?;
    // SAFETY: the symbols exist (the package just made); each function
    // matches its arity.
    unsafe {
        for (name, f) in [
            (c"%NEXT", veles_lisp_next as *const ()),
            (c"%SEND", veles_lisp_send as *const ()),
            (c"%DEAD", veles_lisp_dead as *const ()),
        ] {
            let sym = ffi::ecl_make_symbol(name.as_ptr(), c"VELES-SYS".as_ptr());
            ffi::ecl_def_c_function(sym, f as *const std::os::raw::c_void, 1);
        }
    }
    // The boot form: any error out of the runtime is reported as the
    // engine's last words, and the form returns, so the engine thread
    // ends with the reason (ASCII: it crosses as a base string).
    let runtime = engine.lisp_dir.join("runtime.lisp");
    let veles = engine.lisp_dir.join("veles.lisp");
    let boot = format!(
        r#"(handler-case
             (progn
               (ext:install-bytecodes-compiler)
               (let ((*load-verbose* nil) (*compile-verbose* nil))
                 (handler-bind ((warning #'muffle-warning))
                   (load {runtime})
                   (load {veles})))
               (funcall (intern "MAIN-LOOP" "VELES-RUNTIME") {deadline}))
           (serious-condition (e)
             (veles-sys:%dead
               (coerce (map 'string (lambda (c) (if (< (char-code c) 128) c #\?))
                            (or (ignore-errors (princ-to-string e)) "an unprintable condition"))
                       'simple-base-string))))"#,
        runtime = lisp_string(&runtime.display().to_string()),
        veles = lisp_string(&veles.display().to_string()),
        deadline = invoke_deadline_s(),
    );
    eval(&boot).map(|_| ())
}

/// The signals the gateway handles itself; ECL's boot must not keep a
/// hand on any of them.
pub const GUARDED_SIGNALS: [i32; 5] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGPIPE,
    libc::SIGCHLD,
];

/// The current disposition of each guarded signal.
pub fn signal_dispositions() -> Vec<(i32, libc::sigaction)> {
    GUARDED_SIGNALS
        .iter()
        .map(|&sig| {
            // SAFETY: sigaction with a null `act` only reads.
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
            (sig, old)
        })
        .collect()
}

fn restore_signal_dispositions(saved: &[(i32, libc::sigaction)]) {
    for (sig, act) in saved {
        // SAFETY: putting back a disposition read from this process.
        unsafe { libc::sigaction(*sig, act, std::ptr::null_mut()) };
    }
}

/// Write the two Lisp files the boot loads.
fn write_sources(dir: &Path, runtime_lisp: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, body) in [
        ("runtime.lisp", runtime_lisp),
        ("veles.lisp", crate::VELES_LISP),
    ] {
        let p = dir.join(name);
        std::fs::write(&p, body).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! In-process: the engine as the `lisp` host of the real seam, scripts
    //! under a scratch plugins root, the registrations read back through
    //! core. ECL boots ONCE for the test process (the first test to get
    //! here), so the tests share the engine and take turns (SERIAL).
    use super::*;
    use veles_core::plugins::{LoadedPlugin, PluginManifest};
    use veles_core::scripthost;

    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static STATE: OnceLock<tempfile::TempDir> = OnceLock::new();
    /// The process's signal dispositions before any test booted ECL.
    static SIGNALS_BEFORE: OnceLock<Vec<(i32, usize)>> = OnceLock::new();

    fn handlers(v: &[(i32, libc::sigaction)]) -> Vec<(i32, usize)> {
        v.iter().map(|(s, a)| (*s, a.sa_sigaction)).collect()
    }

    /// A test's runtime, shut down the way the module host shuts an
    /// instance's down: the engine's blocking thread never returns, and a
    /// plain drop of the runtime waits for it for ever. The shutdown is a
    /// drop guard, so a FAILED assertion's unwind takes the same way out
    /// (the Racket host's tests hung on that path, 2026-10-10).
    fn run<F: std::future::Future<Output = ()>>(body: F) {
        struct Background(Option<tokio::runtime::Runtime>);
        impl Drop for Background {
            fn drop(&mut self) {
                if let Some(rt) = self.0.take() {
                    rt.shutdown_background();
                }
            }
        }
        let rt = Background(Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        ));
        rt.0.as_ref().unwrap().block_on(body);
    }

    fn plugin(root: &Path, name: &str, body: &str) -> LoadedPlugin {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.lisp"), body).unwrap();
        LoadedPlugin {
            manifest: PluginManifest {
                name: name.into(),
                kind: "lisp".into(),
                provides_hooks: Vec::new(),
                entry: "plugin.lisp".into(),
                description: String::new(),
                api: None,
            },
            dir: d,
        }
    }

    async fn engine(owner: &'static str) -> Arc<LispEngine> {
        // What the module host does before any module boots: the gateway's
        // own handlers go in first, so the signal check compares real ones.
        veles_core::lifecycle::pin_signal_handlers().expect("pinned");
        SIGNALS_BEFORE.get_or_init(|| handlers(&signal_dispositions()));
        // Read at boot by the Lisp side: short, for the deadline test.
        std::env::set_var("VELES_LISP_INVOKE_DEADLINE_S", "2");
        let state = STATE.get_or_init(|| tempfile::tempdir().unwrap());
        let e = LispEngine::get_or_boot(tokio::runtime::Handle::current(), state.path())
            .await
            .unwrap();
        let host: Arc<dyn ScriptHost> = e.clone();
        veles_core::lifecycle::with_thread_owner(owner, || scripthost::register_host(host));
        e
    }

    /// Dispatch a global tool the way a session would.
    async fn call_tool(name: &str, args: serde_json::Value) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            veles_core::store::Store::open(&tmp.path().join("state.db")).unwrap(),
        ));
        let reg = veles_core::tools::builtin_registry(store, None, "lang-lisp-test");
        reg.dispatch(name, args).await
    }

    fn owned_by(owner: &str) -> Vec<String> {
        veles_core::tools::global_tools_with_owners()
            .into_iter()
            .filter(|(_, o)| *o == Some(owner))
            .map(|(n, _)| n)
            .collect()
    }

    #[test]
    fn a_lisp_plugin_registers_and_answers_through_the_seam() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-lisp-e1").await;
            assert_eq!(e.version(), "26.5.5");
            assert!(veles_core::scripthost::engine_booted("lisp"));
            let p = plugin(
                root.path(),
                "e1-demo",
                r#";; the e1 fixture
(register-tool "e1_echo" "echo"
  (obj "type" "object" "properties" (obj "text" (obj "type" "string")))
  (lambda (args session-id)
    (format nil "e1-tool: ~a for ~a" (gethash "text" args "") session-id)))
(register-command "e1cmd" "a command" (lambda (text) (format nil "e1cmd rendered ~a" text)))
(hook "post_llm_call"
  (lambda (args)
    (when (equal (gethash "content" args) "e1-magic") (obj "decision" "from-e1"))))
(log-line "e1 loaded")
"#,
            );
            let line = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
                .await
                .unwrap();
            assert!(
                line.contains("lisp:e1-demo: loaded") && line.contains("3 callback(s)"),
                "{line}"
            );
            assert_eq!(owned_by("lisp:e1-demo"), vec!["e1_echo".to_string()]);
            let out = call_tool("e1_echo", json!({"text": "hi"})).await;
            assert!(out.starts_with("e1-tool: hi for "), "{out}");
            // Past ASCII both ways, a pair past the BMP among it: the wire
            // is \u escapes, decoded on each side.
            let wide = "żółw → λ 🦀";
            let out = call_tool("e1_echo", json!({ "text": wide })).await;
            assert!(out.starts_with(&format!("e1-tool: {wide} for ")), "{out}");
            let cmd = veles_core::commands::plugin_command("e1cmd").unwrap();
            let rendered = veles_core::commands::render_plugin_async(&cmd, "now")
                .await
                .unwrap();
            assert_eq!(rendered, "e1cmd rendered now");
            let verdict = veles_core::rt::spawn_blocking(|| {
                veles_core::rustplugins::run_hooks("post_llm_call", r#"{"content":"e1-magic"}"#)
            })
            .await
            .unwrap();
            assert_eq!(verdict.as_deref(), Some(r#"{"decision":"from-e1"}"#));
            let none = veles_core::rt::spawn_blocking(|| {
                veles_core::rustplugins::run_hooks("post_llm_call", r#"{"content":"e1-plain"}"#)
            })
            .await
            .unwrap();
            assert_eq!(none, None, "NIL is no opinion");
            // A reload: one tool, the new package answers.
            scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
                .await
                .unwrap();
            assert_eq!(owned_by("lisp:e1-demo").len(), 1);
            assert_eq!(e.loaded(), vec!["e1-demo".to_string()]);
            let gone = scripthost::unload_script("e1-demo").await.unwrap();
            assert!(gone.contains("package deleted"), "{gone}");
            assert!(owned_by("lisp:e1-demo").is_empty());
            assert!(e.loaded().is_empty());
            // A late invoke of the unloaded script is refused by the host.
            let late = ScriptHost::invoke(&*e, "e1-demo", 1, "{}".into()).await;
            assert!(late.unwrap_err().contains("not loaded"));
            scripthost::purge_owner_hosts(Some("module:lang-lisp-e1"));
        });
    }

    #[test]
    fn a_load_that_raises_or_exits_leaves_nothing_registered() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-lisp-e2").await;
            let bad = plugin(
                root.path(),
                "e2-bad",
                r#"(register-tool "e2_tool" "registered, then the load raises" "{}" (lambda (a s) "x"))
(error "e2 nope at the top")
"#,
            );
            let err = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &bad)
                .await
                .unwrap_err();
            assert!(err.contains("e2 nope at the top"), "{err}");
            assert!(e.loaded().is_empty());
            assert!(
                owned_by("lisp:e2-bad").is_empty(),
                "a failed load registers nothing"
            );
            // The Lisp side dropped the script too: the callback the top
            // level registered before raising is gone with it.
            let late = ScriptHost::invoke(&*e, "e2-bad", 1, "{}".into()).await;
            assert!(
                late.unwrap_err().contains("not loaded"),
                "a failed load's script lives on"
            );
            for (name, body) in [("e2-exit", "(ext:exit 3)\n"), ("e2-quit", "(ext:quit 4)\n")] {
                let exits = plugin(root.path(), name, body);
                let err = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &exits)
                    .await
                    .unwrap_err();
                assert!(err.contains("may not exit the gateway"), "{name}: {err}");
            }
            assert!(e.loaded().is_empty());
            scripthost::purge_owner_hosts(Some("module:lang-lisp-e2"));
        });
    }

    #[test]
    fn a_handler_past_the_deadline_is_killed_and_the_script_keeps_working() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-lisp-e3").await;
            // The deadline is 2 s (set in `engine`); the handler would
            // write its marker at 4 s, so the file's absence at 8 s is the
            // kill itself (the watchdog's reply comes either way).
            let marker = root.path().join("e3-late.marker");
            let p = plugin(
                root.path(),
                "e3-slow",
                &format!(
                    r#"(register-tool "e3_sleep" "sleeps past the deadline" "{{}}"
  (lambda (a s) (sleep 4)
    (with-open-file (o "{}" :direction :output :if-exists :supersede) (write-string "late" o))
    "late"))
(register-tool "e3_quick" "answers at once" "{{}}" (lambda (a s) "quick"))
"#,
                    marker.display()
                ),
            );
            scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
                .await
                .unwrap();
            let t0 = Instant::now();
            let out = call_tool("e3_sleep", json!({})).await;
            assert!(out.contains("killed"), "{out}");
            assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());
            // The script's other handlers were not the killed thread.
            assert_eq!(call_tool("e3_quick", json!({})).await, "quick");
            tokio::time::sleep(Duration::from_secs(8).saturating_sub(t0.elapsed())).await;
            assert!(!marker.exists(), "the handler lived on past its deadline");
            scripthost::unload_script("e3-slow").await.unwrap();
            scripthost::purge_owner_hosts(Some("module:lang-lisp-e3"));
        });
    }

    /// The boot touches the process's signal dispositions; the options
    /// keep ECL's hands off the gateway's and the restore puts back
    /// anything else, over the gateway's own (pinned before it, as the
    /// module host does), so a stop still reaches the gateway with ECL up.
    #[test]
    fn the_boot_leaves_the_gateways_signal_handlers_alone() {
        run(async {
            let _serial = SERIAL.lock().await;
            let _e = engine("module:lang-lisp-e4").await;
            let before = SIGNALS_BEFORE.get().cloned().unwrap();
            let after = handlers(&signal_dispositions());
            assert_eq!(after, before, "a disposition changed under the gateway");
            let own = before
                .iter()
                .filter(|(s, h)| {
                    [libc::SIGTERM, libc::SIGINT].contains(s)
                        && *h != libc::SIG_DFL
                        && *h != libc::SIG_IGN
                })
                .count();
            assert_eq!(
                own, 2,
                "the gateway's handlers were not in before the boot: {before:?}"
            );
            let caught = veles_core::lifecycle::caught_signals();
            for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD] {
                assert!(
                    caught.contains(&sig),
                    "signal {sig} is not caught with ECL up: {caught:?}"
                );
            }
            // Rust's own fault handlers stay (ECL's traps are off): a
            // fault on a Rust thread is still a Rust crash, not a thread
            // that silently ends.
            for sig in [libc::SIGSEGV, libc::SIGBUS] {
                // SAFETY: sigaction with a null `act` only reads.
                let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
                let in_libecl = {
                    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
                    // SAFETY: dladdr reads the loaded objects' tables.
                    let found =
                        unsafe { libc::dladdr(old.sa_sigaction as *const libc::c_void, &mut info) };
                    found != 0
                        && !info.dli_fname.is_null()
                        && unsafe { CStr::from_ptr(info.dli_fname) }
                            .to_string_lossy()
                            .contains("libecl")
                };
                assert!(!in_libecl, "signal {sig} went to ECL's handler");
            }
            scripthost::purge_owner_hosts(Some("module:lang-lisp-e4"));
        });
    }

    /// Scripts provide and call services through core's registry: the
    /// provider's gate applies, the caller is named, Rust in-process
    /// reaches the script's handler too, and the provider's unload
    /// withdraws its services (the JS and Racket hosts' test, in Lisp).
    #[test]
    fn scripts_provide_and_call_services_through_the_registry() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-lisp-e5").await;
            let prov = plugin(
                root.path(),
                "e5-prov",
                r#"(register-service "lsptest.add" "adds"
  (lambda (p caller sid)
    (declare (ignore sid))
    (obj "sum" (+ (gethash "a" p) (gethash "b" p)) "caller" caller))
  :cap "any")
(register-service "lsptest.internal" "" (lambda (p caller sid) (declare (ignore p caller sid)) "secret"))
"#,
            );
            scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &prov)
                .await
                .unwrap();
            let v = veles_core::services::call("lsptest.add", json!({"a": 2, "b": 3}))
                .await
                .unwrap();
            assert_eq!(v["sum"], 5, "{v}");
            assert_eq!(
                veles_core::services::provider("lsptest.add").as_deref(),
                Some("lisp:e5-prov")
            );
            let user = plugin(
                root.path(),
                "e5-user",
                r#"(register-command "e5call" "calls services"
  (lambda (text)
    (declare (ignore text))
    (let ((r (service-call "lsptest.add" (obj "a" 1 "b" 1)))
          (refused (handler-case (progn (service-call "lsptest.internal" (obj)) "")
                     (error (e) (princ-to-string e))))
          (listed (sort (remove-if-not (lambda (n) (eql 0 (search "lsptest." n)))
                                       (map 'list (lambda (s) (gethash "name" s)) (services)))
                        #'string<)))
      (json-string (obj "r" r "refused" refused "listed" listed)))))
"#,
            );
            scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &user)
                .await
                .unwrap();
            let cmd = veles_core::commands::plugin_command("e5call").unwrap();
            let out = veles_core::commands::render_plugin_async(&cmd, "")
                .await
                .unwrap();
            let v: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out}"));
            assert_eq!(v["r"]["sum"], 2, "{out}");
            assert_eq!(v["r"]["caller"], "lisp:e5-user", "{out}");
            assert!(
                v["refused"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("internal service"),
                "{out}"
            );
            assert_eq!(
                v["listed"],
                json!(["lsptest.add", "lsptest.internal"]),
                "{out}"
            );
            scripthost::unload_script("e5-prov").await.unwrap();
            assert!(!veles_core::services::available("lsptest.add"));
            scripthost::unload_script("e5-user").await.unwrap();
            scripthost::purge_owner_hosts(Some("module:lang-lisp-e5"));
        });
    }

    /// What the spike crashed or hung on, from a script: a recursion
    /// without end (in a handler and in a thread of its own) is a stack
    /// overflow condition, an error nobody handles in a raw thread is a
    /// log line, a float division by zero is infinity; the gateway — this
    /// process — stays up, and the script keeps answering.
    #[test]
    fn a_runaway_script_is_a_lisp_error_never_a_dead_gateway() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-lisp-e6").await;
            let p = plugin(
                root.path(),
                "e6-wild",
                r#"(defun down (n) (1+ (down n)))
(register-tool "e6_deep" "recurses for ever" "{}" (lambda (a s) (down 1)))
(register-tool "e6_deep_thread" "recurses in a thread of its own" "{}"
  (lambda (a s)
    (let* ((result nil)
           (p (spawn "deep" (lambda () (setf result (handler-case (down 1) (storage-condition (c) (format nil "caught: ~a" (type-of c)))))))))
      (mp:process-join p)
      result)))
(register-tool "e6_unhandled" "an error nobody handles, in a raw thread" "{}"
  (lambda (a s)
    (mp:process-join (mp:process-run-function "e6-raw" (lambda () (error "e6 unhandled"))))
    "survived"))
(register-tool "e6_inf" "divides by zero" "{}" (lambda (a s) (princ-to-string (/ 1.0 0.0))))
(register-tool "e6_ok" "answers" "{}" (lambda (a s) "ok"))
"#,
            );
            scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
                .await
                .unwrap();
            let deep = call_tool("e6_deep", json!({})).await;
            assert!(deep.to_lowercase().contains("stack"), "{deep}");
            let thread = call_tool("e6_deep_thread", json!({})).await;
            assert!(thread.contains("caught"), "{thread}");
            assert_eq!(call_tool("e6_unhandled", json!({})).await, "survived");
            let inf = call_tool("e6_inf", json!({})).await;
            assert!(inf.to_lowercase().contains("infinity"), "{inf}");
            assert_eq!(call_tool("e6_ok", json!({})).await, "ok");
            assert!(matches!(e.state(), EngineState::Up(_)), "{:?}", e.state());
            scripthost::unload_script("e6-wild").await.unwrap();
            scripthost::purge_owner_hosts(Some("module:lang-lisp-e6"));
        });
    }

    /// An unload ends what the script started: a thread it made with
    /// `spawn` dies before it can write its marker, and its package is
    /// gone (another script looks it up by name).
    #[test]
    fn an_unload_kills_the_scripts_threads_and_deletes_its_package() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-lisp-e7").await;
            let marker = root.path().join("e7-late.marker");
            let tick = plugin(
                root.path(),
                "e7-tick",
                &format!(
                    r#"(spawn "late" (lambda () (sleep 3)
  (with-open-file (o "{}" :direction :output :if-exists :supersede) (write-string "late" o))))
(register-tool "e7_alive" "answers" "{{}}" (lambda (a s) "alive"))
"#,
                    marker.display()
                ),
            );
            let probe = plugin(
                root.path(),
                "e7-probe",
                r#"(register-tool "e7_probe" "is e7-tick's package there?" "{}"
  (lambda (a s) (if (find-package "VELES-SCRIPT/E7-TICK") "present" "gone")))
"#,
            );
            for p in [&tick, &probe] {
                scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, p)
                    .await
                    .unwrap();
            }
            assert_eq!(call_tool("e7_probe", json!({})).await, "present");
            let t0 = Instant::now();
            let gone = scripthost::unload_script("e7-tick").await.unwrap();
            assert!(gone.contains("1 thread(s) killed"), "{gone}");
            assert_eq!(call_tool("e7_probe", json!({})).await, "gone");
            tokio::time::sleep(Duration::from_secs(5).saturating_sub(t0.elapsed())).await;
            assert!(!marker.exists(), "the script's thread outlived its unload");
            scripthost::unload_script("e7-probe").await.unwrap();
            scripthost::purge_owner_hosts(Some("module:lang-lisp-e7"));
        });
    }

    /// The wire is ASCII both ways: every non-ASCII character a `\u`
    /// escape, a pair of them past the BMP.
    #[test]
    fn ascii_json_escapes_everything_past_127() {
        let v = json!({"t": "x", "s": "żółw → 🦀", "n": 1});
        let a = ascii_json(&v);
        assert!(a.is_ascii(), "{a}");
        assert!(a.contains("\\ud83e\\udd80"), "{a}");
        let back: Value = serde_json::from_str(&a).unwrap();
        assert_eq!(back, v);
    }
}
