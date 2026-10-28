//! The engine: Racket CS booted ONCE per process on one OS thread, and a
//! mailbox between that thread and the rest of the gateway.
//!
//! Racket CS is callable from the OS thread that booted it and no other
//! (a call from a second thread hangs; measured 2026-10-10), so the
//! engine thread is the only one that ever enters Racket: it boots,
//! then instantiates `veles/boot`, whose body is the loop in
//! `runtime.rkt`, which never returns. Every request (`load`, `unload`,
//! `invoke`) is a JSON message pushed into the mailbox plus one byte
//! down a pipe; the loop reads the byte, polls the mailbox dry through
//! `veles_racket_poll`, and answers through `veles_racket_send`.
//! Handlers run as Racket (green) threads under their script's
//! custodian: a slow one blocks nothing, one past its deadline is
//! killed, and an unload is a custodian shutdown. A host call a handler
//! makes (`register_tool`, `http_get`, …) comes back the same way, as a
//! request with an id; the Rust side runs it on the instance runtime
//! and pushes the reply into the mailbox.
//!
//! The engine outlives the module instance: `stop` unloads the scripts
//! and leaves Racket booted (it cannot be booted twice), the engine
//! thread keeps the module mapped (the module host reports it `kept
//! mapped`, by design), and the next `start` of the same mapping reuses
//! it with the new instance's runtime. A SECOND copy of this module (a
//! reload after the file changed) finds core's boot mark taken and
//! refuses to start: replacing the Racket host is a gateway restart.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use veles_runtime::tokio;
use veles_runtime::tokio::sync::{oneshot, watch};
use veles_runtime::veles_core;
use veles_runtime::veles_core::scripthost::{BoxFut, ScriptCx, ScriptHost, ScriptRef};

use crate::ffi;

/// How long a `load` may take: the module's compile plus whatever its
/// top level does (each registration is a host call and back).
const LOAD_DEADLINE: Duration = Duration::from_secs(90);
/// The boot handshake: racket_boot, racket/base and the runtime module
/// (~100 ms measured; a cold page cache reads 50 MB of boot files).
const BOOT_DEADLINE: Duration = Duration::from_secs(30);
const UNLOAD_DEADLINE: Duration = Duration::from_secs(10);

/// Why an `ask` has no answer: Racket said no (`Replied`: the Racket side
/// ran its own cleanup before answering, a failed load's custodian
/// shutdown among it), or nothing came back at all (`NoAnswer`: the
/// deadline passed, or the engine is dead), when whatever Racket would
/// have done is this side's to do.
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

/// The seconds the Racket side lets a handler run before killing it;
/// the seam's own deadlines (300/60/30 s) are what a caller sees first.
fn invoke_deadline_s() -> u64 {
    std::env::var("VELES_RACKET_INVOKE_DEADLINE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Where Racket is: `VELES_RACKET_HOME` at run time, else the install
/// the crate was built against.
pub fn racket_home() -> PathBuf {
    std::env::var("VELES_RACKET_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("VELES_RACKET_HOME")))
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineState {
    Booting,
    /// Up, with Racket's version.
    Up(String),
    Dead(String),
}

pub struct RacketEngine {
    mailbox: Mutex<VecDeque<String>>,
    wake_w: i32,
    /// The pipe's read end is Racket's (its number is baked into
    /// boot.rkt); held here so it stays open for the engine's life.
    _wake_r: i32,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<String, AskError>>>>,
    /// The scripts the seam handed this host, by name: their `cx` is
    /// what a host call from Racket code goes through.
    scripts: Mutex<BTreeMap<String, Arc<ScriptCx>>>,
    /// The CURRENT instance's runtime: host calls are spawned onto it.
    handle: Mutex<Option<tokio::runtime::Handle>>,
    next_id: AtomicU64,
    state: watch::Sender<EngineState>,
    /// What boot.rkt reported before the loop returned (an error that
    /// escaped the runtime): the engine thread's death reason.
    last_words: Mutex<Option<String>>,
    started: Instant,
    home: PathBuf,
    collects: PathBuf,
}

/// The one engine of this process (this mapping of the module).
static ENGINE: OnceLock<Arc<RacketEngine>> = OnceLock::new();

impl RacketEngine {
    /// The engine of this process: booted on the first call (the caller
    /// waits for Racket's handshake), reused by every later one with the
    /// new instance's runtime handle. `state_dir` is the module's state
    /// dir, where the `veles` collection is written.
    pub async fn get_or_boot(
        handle: tokio::runtime::Handle,
        state_dir: &Path,
    ) -> Result<Arc<RacketEngine>, String> {
        Self::get_or_boot_with(handle, state_dir, crate::RUNTIME_RKT).await
    }

    /// `get_or_boot` with the runtime's Racket source given, for a test
    /// that boots a broken one (in its own process: Racket boots once).
    pub async fn get_or_boot_with(
        handle: tokio::runtime::Handle,
        state_dir: &Path,
        runtime_rkt: &str,
    ) -> Result<Arc<RacketEngine>, String> {
        if let Some(e) = ENGINE.get() {
            *e.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
            return match e.state() {
                EngineState::Dead(why) => Err(format!("the Racket engine died earlier: {why}")),
                _ => Ok(Arc::clone(e)),
            };
        }
        if !veles_core::scripthost::mark_engine_booted(crate::KIND) {
            return Err(
                "lang-racket is pinned: Racket CS boots once per process and another copy of \
                 this module booted it; replacing the Racket host is a gateway restart"
                    .into(),
            );
        }
        let home = racket_home();
        for f in [
            "lib/racket/petite.boot",
            "lib/racket/scheme.boot",
            "lib/racket/racket.boot",
            "share/racket/collects",
        ] {
            if !home.join(f).exists() {
                return Err(format!(
                    "VELES_RACKET_HOME={}: no {f} (a Racket CS 9.3 install is expected)",
                    home.display()
                ));
            }
        }
        let collects = state_dir.join("collects");
        let (wake_r, wake_w) = pipe()?;
        write_collection(&collects, wake_r, runtime_rkt)?;
        let (state, _) = watch::channel(EngineState::Booting);
        let engine = Arc::new(RacketEngine {
            mailbox: Mutex::new(VecDeque::new()),
            wake_w,
            _wake_r: wake_r,
            waiting: Mutex::new(HashMap::new()),
            scripts: Mutex::new(BTreeMap::new()),
            handle: Mutex::new(Some(handle.clone())),
            next_id: AtomicU64::new(0),
            state,
            last_words: Mutex::new(None),
            started: Instant::now(),
            home,
            collects,
        });
        let e = ENGINE.get_or_init(|| Arc::clone(&engine));
        let me = Arc::clone(e);
        // The engine thread is one of the instance runtime's blocking
        // threads on purpose: the module host counts it, so the teardown
        // of this instance reports the module kept mapped (which it is).
        drop(handle.spawn_blocking(move || engine_thread(me)));
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
            Ok(Err(why)) => Err(format!("Racket failed to boot: {why}")),
            Err(_) => Err(format!(
                "Racket did not answer the boot handshake within {BOOT_DEADLINE:?}"
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

    /// Racket's version once up.
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

    /// Unload every script (the module's stop); Racket stays booted.
    pub async fn unload_all(&self) -> usize {
        let names = self.loaded();
        for name in &names {
            let _ = ScriptHost::unload(self, name).await;
        }
        names.len()
    }

    /// Into the mailbox, and one byte down the pipe.
    fn push(&self, msg: Value) {
        self.mailbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(msg.to_string());
        let b = [1u8];
        // SAFETY: a one-byte write to the non-blocking pipe this engine
        // owns; EAGAIN (the pipe full of wake bytes) means Racket has
        // more than enough reason to wake already.
        unsafe { libc::write(self.wake_w, b.as_ptr() as *const libc::c_void, 1) };
    }

    /// A request with an id and a slot for its reply.
    fn request(&self, mut msg: Value) -> (u64, oneshot::Receiver<Result<String, AskError>>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        msg["id"] = json!(id);
        if let EngineState::Dead(why) = self.state() {
            let _ = tx.send(Err(AskError::NoAnswer(format!(
                "the Racket engine is dead: {why}"
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
                "the Racket engine dropped the request".into(),
            )),
            Err(_) => {
                self.waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(AskError::NoAnswer(format!(
                    "no answer from Racket within {deadline:?}"
                )))
            }
        }
    }

    /// Everything Racket sends, on the engine thread (`veles_racket_send`).
    fn on_message(&self, text: &str) {
        let m: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                veles_core::module::log(
                    veles_runtime::tracing::Level::WARN,
                    &format!("lang-racket: unreadable message from Racket ({e}): {text}"),
                );
                return;
            }
        };
        match m["t"].as_str().unwrap_or("") {
            "booted" => {
                let v = m["version"].as_str().unwrap_or("?").to_string();
                let _ = self.state.send(EngineState::Up(v));
            }
            // The runtime's last words: boot.rkt caught an error that
            // escaped the runtime and is about to return.
            "dead" => {
                let why = m["why"].as_str().unwrap_or("?").to_string();
                *self.last_words.lock().unwrap_or_else(|p| p.into_inner()) = Some(why);
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
                    format!("lang-racket: {text}")
                } else {
                    format!("racket:{name}: {text}")
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
                &format!("lang-racket: unknown message kind `{other}` from Racket"),
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
                "racket:{name}: not loaded (a host call after its unload?)"
            )))),
            _ => self.push(reply(Err(
                "the Racket host has no instance runtime right now".into(),
            ))),
        }
    }
}

impl ScriptHost for RacketEngine {
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
                    // The load raised (or tried to exit): the Racket side
                    // shut the script's custodian and dropped its entry
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

/// Racket → Rust: the next mailbox message as a C string (freed with
/// `veles_racket_free`), or null when the mailbox is empty.
pub extern "C" fn veles_racket_poll() -> *mut c_char {
    let Some(e) = ENGINE.get() else {
        return std::ptr::null_mut();
    };
    let next = e
        .mailbox
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pop_front();
    match next.and_then(|s| CString::new(s).ok()) {
        Some(c) => c.into_raw(),
        None => std::ptr::null_mut(),
    }
}

/// Racket → Rust: give a `veles_racket_poll` string back.
///
/// # Safety
/// `p` is null or a pointer `veles_racket_poll` returned, handed back
/// exactly once (the runtime's `free-raw`); the engine thread is the
/// only caller.
pub unsafe extern "C" fn veles_racket_free(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Racket → Rust: a message (a reply, a host call, a log line, the
/// boot handshake).
///
/// # Safety
/// `msg` is null or a NUL-terminated string that lives for the call
/// (`_string/utf-8` of ffi/unsafe); the engine thread is the only
/// caller.
pub unsafe extern "C" fn veles_racket_send(msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let Some(e) = ENGINE.get() else {
        return;
    };
    let text = CStr::from_ptr(msg).to_string_lossy().into_owned();
    e.on_message(&text);
}

/// The whole life of the engine thread: boot, the loop, and — should the
/// loop ever return — the death notice.
fn engine_thread(engine: Arc<RacketEngine>) {
    let t0 = Instant::now();
    let why = match boot_and_run(&engine) {
        // The loop returned: boot.rkt reports the error it caught before
        // it does (a runtime that did not even read), and that is why.
        Ok(()) => engine
            .last_words
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| "the Racket loop returned".to_string()),
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
            "the Racket engine died: {why}"
        ))));
    }
    veles_core::module::log(
        veles_runtime::tracing::Level::ERROR,
        &format!(
            "lang-racket: the engine thread ended after {:.0?}: {why}",
            t0.elapsed()
        ),
    );
}

fn cstring(p: PathBuf) -> Result<CString, String> {
    CString::new(p.to_string_lossy().into_owned()).map_err(|e| format!("{}: {e}", p.display()))
}

/// Boot Racket on this thread and instantiate `veles/boot`, which runs
/// the loop and never returns.
fn boot_and_run(engine: &RacketEngine) -> Result<(), String> {
    let home = &engine.home;
    let boot1 = cstring(home.join("lib/racket/petite.boot"))?;
    let boot2 = cstring(home.join("lib/racket/scheme.boot"))?;
    let boot3 = cstring(home.join("lib/racket/racket.boot"))?;
    // `collects_dir` is not a C string to Racket: boot.c's parse_coldirs
    // reads it as NUL-separated directories ended by a SECOND NUL, so a
    // plain C string sends it past its terminator into whatever follows
    // (a SIGSEGV in strlen on the second run, after the first had been
    // lucky; 2026-10-10). Two NULs, kept alive with the rest.
    let collects = {
        let mut v = home
            .join("share/racket/collects")
            .to_string_lossy()
            .into_owned()
            .into_bytes();
        v.extend_from_slice(b"\0\0");
        v
    };
    let config = cstring(home.join("etc/racket"))?;
    let exec = CString::new("veles").map_err(|e| e.to_string())?;
    // Our collection ahead of the install's; an empty entry stands for
    // the default (Racket reads PLTCOLLECTS at boot).
    std::env::set_var("PLTCOLLECTS", format!("{}:", engine.collects.display()));
    // SAFETY: an all-zero struct is the documented default for every
    // field; the strings are leaked below because Racket keeps the
    // pointers it was booted with.
    let mut args: ffi::BootArgs = unsafe { std::mem::zeroed() };
    args.boot1_path = boot1.as_ptr();
    args.boot2_path = boot2.as_ptr();
    args.boot3_path = boot3.as_ptr();
    args.exec_file = exec.as_ptr();
    args.collects_dir = collects.as_ptr() as *const std::os::raw::c_char;
    args.config_dir = config.as_ptr();
    // The gateway's signal handling (tokio's SIGTERM/SIGINT/SIGCHLD
    // drivers, the fatal hook) is the process's, not Racket's: whatever
    // the boot installs is put back. (The first run showed it: a test
    // process with Racket up ignored SIGTERM, 2026-10-10.)
    // Right only because the gateway's handlers are already in: the module
    // host pins them before any module's code runs. One that went in during
    // this boot would be put back to SIG_DFL by the restore below (the VPS
    // gateway ignored podman's stop that way, 2026-10-10).
    let before = signal_dispositions();
    // SAFETY: called once per process (core's boot mark), on the thread
    // that stays Racket's for the process's life.
    unsafe { ffi::racket_boot(&mut args) };
    std::mem::forget((boot1, boot2, boot3, collects, config, exec));
    restore_signal_dispositions(&before);
    let base = CString::new("racket/base").map_err(|e| e.to_string())?;
    let boot_mod = CString::new("veles/boot").map_err(|e| e.to_string())?;
    // SAFETY: Racket is booted on this thread; each symbol goes straight
    // into the call that takes it (nothing is held across a collection).
    unsafe {
        ffi::racket_namespace_require(ffi::Sstring_to_symbol(base.as_ptr()));
        ffi::racket_dynamic_require(ffi::Sstring_to_symbol(boot_mod.as_ptr()), ffi::SFALSE);
    }
    Ok(())
}

/// The signals the gateway handles itself; Racket's boot must not keep
/// a hand on any of them. Subprocesses are the one thing this costs a
/// plugin: SIGCHLD stays tokio's, so `system`/`process` from Racket is
/// unsupported (use the engine's tools).
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

/// A non-blocking pipe: Rust writes the wake bytes, Racket reads them.
fn pipe() -> Result<(i32, i32), String> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 fills the two-element array it is given.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(format!(
            "pipe2 for the Racket wake: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((fds[0], fds[1]))
}

/// Write the `veles` collection the engine runs (`runtime.rkt`,
/// `main.rkt`) and the boot module that binds this process's addresses
/// and starts the loop.
/// `veles/boot`, the runtime's entry, written at boot with this
/// process's addresses. An error that escapes the runtime (one that
/// does not even read) is reported as the engine's last words and the
/// module returns, so the engine thread ends with the reason: Racket's
/// own escape handler at boot would EXIT the gateway, with status 0
/// (a mutant run read exactly that as green, 2026-10-10).
const BOOT_RKT: &str = r#"#lang racket/base
;; Written by the lang-racket host at boot: this process's addresses.
(require ffi/unsafe json)
(define (report why)
  ((cast (cast @SEND@ _intptr _pointer) _pointer (_fun _string/utf-8 -> _void))
   (jsexpr->string (hash 't "dead" 'why why))))
(with-handlers ([(lambda (e) #t)
                 (lambda (e) (report (if (exn? e) (exn-message e) (format "raised: ~e" e))))])
  (let ([init (dynamic-require 'veles/runtime 'veles-runtime-init!)]
        [loop (dynamic-require 'veles/runtime 'veles-main-loop)])
    (init @POLL@ @SEND@ @FREE@ @FD@ @DEADLINE@)
    (loop)))
"#;

fn write_collection(collects: &Path, wake_fd: i32, runtime_rkt: &str) -> Result<(), String> {
    let dir = collects.join("veles");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let w = |name: &str, body: String| -> Result<(), String> {
        let p = dir.join(name);
        std::fs::write(&p, body).map_err(|e| format!("{}: {e}", p.display()))
    };
    w("runtime.rkt", runtime_rkt.to_string())?;
    w("main.rkt", crate::VELES_RKT.to_string())?;
    let boot = BOOT_RKT
        .replace(
            "@POLL@",
            &(veles_racket_poll as *const () as usize).to_string(),
        )
        .replace(
            "@SEND@",
            &(veles_racket_send as *const () as usize).to_string(),
        )
        .replace(
            "@FREE@",
            &(veles_racket_free as *const () as usize).to_string(),
        )
        .replace("@FD@", &wake_fd.to_string())
        .replace("@DEADLINE@", &invoke_deadline_s().to_string());
    w("boot.rkt", boot)?;
    // A stale compiled tree beside the sources would win over them.
    let _ = std::fs::remove_dir_all(dir.join("compiled"));
    Ok(())
}

#[cfg(test)]
mod tests {
    //! In-process: the engine as the `racket` host of the real seam,
    //! scripts under a scratch plugins root, the registrations read back
    //! through core. Racket boots ONCE for the test process (the first
    //! test boots it, the rest reuse it), and the seam has ONE host per
    //! kind, so the tests run one at a time. (The module form is proven
    //! by `veles module-host-test conformance-racket`.)
    use super::*;
    use veles_core::plugins::{LoadedPlugin, PluginManifest};
    use veles_core::scripthost;

    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static STATE: OnceLock<tempfile::TempDir> = OnceLock::new();
    /// The process's signal dispositions before any test booted Racket.
    static SIGNALS_BEFORE: OnceLock<Vec<(i32, usize)>> = OnceLock::new();

    fn handlers(v: &[(i32, libc::sigaction)]) -> Vec<(i32, usize)> {
        v.iter().map(|(s, a)| (*s, a.sa_sigaction)).collect()
    }

    /// A test's runtime, shut down the way the module host shuts an
    /// instance's down. The engine thread is a blocking thread of the
    /// runtime that booted it and never returns: a plain drop of that
    /// runtime waits for it for ever (tokio's blocking pool has no
    /// timeout there), which is how the first run of these tests hung
    /// for two hours with every assertion already passed. The shutdown
    /// is a drop guard, so a FAILED assertion's unwind takes the same
    /// way out: a plain drop on that path hung each red test for the
    /// mutant run's whole 600 s, and one that ignored SIGTERM (the
    /// signal mutant) for eight hours (2026-10-10).
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
        std::fs::write(d.join("plugin.rkt"), body).unwrap();
        LoadedPlugin {
            manifest: PluginManifest {
                name: name.into(),
                kind: "racket".into(),
                provides_hooks: Vec::new(),
                entry: "plugin.rkt".into(),
                description: String::new(),
                api: None,
            },
            dir: d,
        }
    }

    async fn engine(owner: &'static str) -> Arc<RacketEngine> {
        // What the module host does before any module boots: the gateway's
        // own handlers go in first, so the check compares real handlers
        // (SIG_DFL put back over SIG_DFL proved nothing, 2026-10-10).
        veles_core::lifecycle::pin_signal_handlers().expect("pinned");
        SIGNALS_BEFORE.get_or_init(|| handlers(&signal_dispositions()));
        // Read at boot by the Racket side: short, for the deadline test.
        std::env::set_var("VELES_RACKET_INVOKE_DEADLINE_S", "2");
        let state = STATE.get_or_init(|| tempfile::tempdir().unwrap());
        let e = RacketEngine::get_or_boot(tokio::runtime::Handle::current(), state.path())
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
        let reg = veles_core::tools::builtin_registry(store, None, "lang-racket-test");
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
    fn a_racket_plugin_registers_and_answers_through_the_seam() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-racket-e1").await;
            assert_eq!(e.version(), "9.3");
            assert!(veles_core::scripthost::engine_booted("racket"));
            let p = plugin(
                root.path(),
                "e1-demo",
                r#"#lang racket/base
    (require veles json)
    (register-tool "e1_echo" "echo"
      (hash 'type "object" 'properties (hash 'text (hash 'type "string")))
      (lambda (args session-id) (format "e1-tool: ~a for ~a" (hash-ref args 'text "") session-id)))
    (register-command "e1cmd" "a command" (lambda (text) (format "e1cmd rendered ~a" text)))
    (hook "post_llm_call"
      (lambda (args)
        (if (equal? (hash-ref args 'content #f) "e1-magic") (hash 'decision "from-e1") (void))))
    (veles-log "e1 loaded")
    "#,
            );
            let line = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
                .await
                .unwrap();
            assert!(
                line.contains("racket:e1-demo: loaded") && line.contains("3 callback(s)"),
                "{line}"
            );
            assert_eq!(owned_by("racket:e1-demo"), vec!["e1_echo".to_string()]);
            let out = call_tool("e1_echo", json!({"text": "hi"})).await;
            assert!(out.starts_with("e1-tool: hi for "), "{out}");
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
            assert_eq!(none, None, "(void) is no opinion");
            // A reload: one tool, the new namespace answers.
            scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
                .await
                .unwrap();
            assert_eq!(owned_by("racket:e1-demo").len(), 1);
            assert_eq!(e.loaded(), vec!["e1-demo".to_string()]);
            let gone = scripthost::unload_script("e1-demo").await.unwrap();
            assert!(gone.contains("custodian shut down"), "{gone}");
            assert!(owned_by("racket:e1-demo").is_empty());
            assert!(e.loaded().is_empty());
            // A late invoke of the unloaded script is refused by the host.
            let late = ScriptHost::invoke(&*e, "e1-demo", 1, "{}".into()).await;
            assert!(late.unwrap_err().contains("not loaded"));
            scripthost::purge_owner_hosts(Some("module:lang-racket-e1"));
        });
    }

    #[test]
    fn a_load_that_raises_or_exits_leaves_nothing_registered() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-racket-e2").await;
            let bad = plugin(
                root.path(),
                "e2-bad",
                r#"#lang racket/base
    (require veles)
    (register-tool "e2_tool" "registered, then the load raises" (hash 'type "object") (lambda (a s) "x"))
    (error 'boom "e2 nope at the top")
    "#,
            );
            let err = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &bad)
                .await
                .unwrap_err();
            assert!(err.contains("e2 nope at the top"), "{err}");
            assert!(e.loaded().is_empty());
            assert!(
                owned_by("racket:e2-bad").is_empty(),
                "a failed load registers nothing"
            );
            // The Racket side dropped the namespace too: the callback the
            // top level registered before raising is gone with it.
            let late = ScriptHost::invoke(&*e, "e2-bad", 1, "{}".into()).await;
            assert!(
                late.unwrap_err().contains("not loaded"),
                "a failed load's namespace lives on"
            );
            let exits = plugin(root.path(), "e2-exit", "#lang racket/base\n(exit 3)\n");
            let err = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &exits)
                .await
                .unwrap_err();
            assert!(err.contains("may not exit the gateway"), "{err}");
            assert!(e.loaded().is_empty());
            scripthost::purge_owner_hosts(Some("module:lang-racket-e2"));
        });
    }

    #[test]
    fn a_handler_past_the_deadline_is_killed_and_the_script_keeps_working() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-racket-e3").await;
            // The deadline is 2 s (set in `engine`); the handler would
            // write its marker at 4 s, so the file's absence at 8 s is
            // the kill itself (the watchdog's reply comes either way).
            let marker = root.path().join("e3-late.marker");
            let p = plugin(
                root.path(),
                "e3-slow",
                &format!(
                    r#"#lang racket/base
    (require veles)
    (register-tool "e3_sleep" "sleeps past the deadline" (hash 'type "object")
      (lambda (a s) (sleep 4) (with-output-to-file "{}" (lambda () (display "late"))) "late"))
    (register-tool "e3_quick" "answers at once" (hash 'type "object") (lambda (a s) "quick"))
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
            scripthost::purge_owner_hosts(Some("module:lang-racket-e3"));
        });
    }

    /// Scripts provide and call services through core's registry: the
    /// provider's gate applies, the caller is named, Rust in-process
    /// reaches the script's handler too, and the provider's unload
    /// withdraws its services (the JS host's test, in Racket).
    #[test]
    fn scripts_provide_and_call_services_through_the_registry() {
        run(async {
            let _serial = SERIAL.lock().await;
            let root = tempfile::tempdir().unwrap();
            let e = engine("module:lang-racket-e5").await;
            let prov = plugin(
                root.path(),
                "e5-prov",
                r#"#lang racket/base
    (require veles)
    (register-service "rktest.add" "adds" #:cap "any"
      (lambda (p caller sid) (hash 'sum (+ (hash-ref p 'a) (hash-ref p 'b)) 'caller caller)))
    (register-service "rktest.internal" "" (lambda (p caller sid) "secret"))
    "#,
            );
            scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &prov)
                .await
                .unwrap();
            let v = veles_core::services::call("rktest.add", json!({"a": 2, "b": 3}))
                .await
                .unwrap();
            assert_eq!(v["sum"], 5, "{v}");
            assert_eq!(
                veles_core::services::provider("rktest.add").as_deref(),
                Some("racket:e5-prov")
            );
            let user = plugin(
                root.path(),
                "e5-user",
                r#"#lang racket/base
    (require veles json)
    (register-command "e5call" "calls services"
      (lambda (text)
        (define r (service-call "rktest.add" (hash 'a 1 'b 1)))
        (define refused
          (with-handlers ([exn:fail? (lambda (e) (exn-message e))])
            (service-call "rktest.internal" (hash))
            ""))
        (define listed
          (sort (filter (lambda (n) (regexp-match? #rx"^rktest[.]" n))
                        (map (lambda (s) (hash-ref s 'name)) (services)))
                string<?))
        (jsexpr->string (hash 'r r 'refused refused 'listed listed))))
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
            assert_eq!(v["r"]["caller"], "racket:e5-user", "{out}");
            assert!(
                v["refused"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("internal service"),
                "{out}"
            );
            assert_eq!(
                v["listed"],
                json!(["rktest.add", "rktest.internal"]),
                "{out}"
            );
            scripthost::unload_script("e5-prov").await.unwrap();
            assert!(!veles_core::services::available("rktest.add"));
            scripthost::unload_script("e5-user").await.unwrap();
            scripthost::purge_owner_hosts(Some("module:lang-racket-e5"));
        });
    }

    /// The boot touches the process's signal dispositions (SIGTERM was
    /// ignored by the first test process); they are put back over the
    /// gateway's own, pinned before it as the module host does, so the
    /// gateway's own handling (a `podman stop`, a systemd stop) keeps
    /// working with Racket up.
    #[test]
    fn the_boot_leaves_the_gateways_signal_handlers_alone() {
        run(async {
            let _serial = SERIAL.lock().await;
            let _e = engine("module:lang-racket-e4").await;
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
            // The kernel's own list with Racket up: a stop still reaches the
            // gateway (SigCgt in /proc/self/status).
            let caught = veles_core::lifecycle::caught_signals();
            for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD] {
                assert!(
                    caught.contains(&sig),
                    "signal {sig} is not caught with Racket up: {caught:?}"
                );
            }
            scripthost::purge_owner_hosts(Some("module:lang-racket-e4"));
        });
    }
}
