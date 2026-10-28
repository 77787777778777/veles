//! One lock for the process-global state tests mutate, and one
//! thread-scoped override for the directories.
//!
//! Several test modules call `std::env::set_var("VELES_HOME", …)` and
//! `set_current_dir`. Those are per-PROCESS, and cargo runs tests in
//! threads, so any two such tests race unless they serialize.
//!
//! Serializing was the old answer, and it was not enough: the lock
//! bounds the WRITES, but a read buried four layers under an agent turn
//! (`config::live()` mid-message) is not holding anybody's test lock,
//! and answered from whatever `VELES_HOME` happened to be set at that
//! instant. Directories are therefore pinned per-THREAD now —
//! `config::DirOverride`, installed by [`scratch_home`] and
//! [`scratch_xdg`] — and the env var survives only as the net for
//! readers that run off-thread (`spawn_blocking`, child processes).
//!
//! There used to be FOUR independent mutexes for this — memory's
//! (shared by acl/prompt/delegate/agents), plus private ones in
//! profiles.rs, checkpoints.rs and gateway.rs. Separate mutexes do not
//! serialize against each other, so a `profiles` test could repoint
//! VELES_HOME while a `memory` test was mid-assertion against its own.
//! Worse, `memory::tests::scratch()` bound the guard to a local, so it
//! unlocked when the fixture RETURNED — before the test body ran at all.
//!
//! Under load that produced exactly the kind of intermittent failure
//! that teaches people to re-run CI instead of trusting it: agents::,
//! delegate:: and memory:: failing together, roughly one run in six.
//!
//! Rules:
//!   1. There is one lock. Take it from here, never declare another.
//!      "Here" means THIS PROCESS: cargo gives each crate its own test
//!      binary, so veles-irc's tests are a separate process and need
//!      their own guard. Two locks in one process is the bug; one lock
//!      per process is the requirement.
//!   2. HOLD THE GUARD FOR THE WHOLE TEST. Return it from fixtures and
//!      bind it in the test body (`let _g = …`), never `let _ = …`.
//!   3. A global keyed by a STRING is the same hazard with no lock to
//!      take: two tests that park state under one literal each see the
//!      other's. Take the key from [`chat_key`] (the thread name libtest
//!      already made unique), and take the lock for anything a test
//!      WRITES that a neighbour reads — the ask registry's ACL snapshot
//!      is one.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// The real process-plugin fixture built by every full test gate.
///
/// A binary-only dev-dependency does not provide CARGO_BIN_EXE_* to a
/// different crate's library unit tests. Locate the explicitly built
/// sibling in this test binary's own profile instead. This works for
/// custom target dirs and Windows' .exe suffix, and never silently skips
/// the real describe/process-host coverage.
pub fn process_plugin_echo() -> PathBuf {
    let current = std::env::current_exe().expect("current test executable");
    let profile = current
        .parent()
        .and_then(|deps| deps.parent())
        .expect("test executable in a Cargo profile/deps directory");
    let exe = profile.join(format!("veles-plugin-echo{}", std::env::consts::EXE_SUFFIX));
    assert!(
        exe.is_file(),
        "real process-plugin fixture is missing: {}. Build it with `cargo build -p veles-plugin-echo` in the same target/profile as the tests",
        exe.display()
    );
    exe
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialize a test that mutates VELES_HOME or the working directory.
/// Poison-recovering: one failed test must not cascade into every other
/// test that touches the environment.
///
/// Taking it also puts the process's user envs offline
/// (`user_envs::set_test_offline`): a test's home is scratch, so a box
/// keyed to it would outlive the test on the developer's real podman.
pub fn lock() -> MutexGuard<'static, ()> {
    crate::user_envs::set_test_offline();
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The lock PLUS automatic restoration of the working directory.
///
/// `set_current_dir` is process-global like the env vars, but it has a
/// nastier failure mode: a test that chdir's into a temp directory and
/// then deletes that directory leaves the WHOLE PROCESS sitting in a
/// path that no longer exists. Everything afterwards inherits it, and
/// `std::process::Command` children inherit it too.
///
/// agents:: did exactly this — chdir into a temp dir, then delete it —
/// leaving the process in a path that no longer exists for every test
/// that followed.
///
/// (Measured, so the record is straight: `git -C <repo>` still works
/// fine from a deleted cwd, so this is NOT the cause of the
/// intermittent memory:: failure that prompted the audit. It is simply
/// a state no test should leave behind.)
///
/// Take this instead of [`lock`] in any test that changes directory.
/// Restoration happens on drop, so it cannot be forgotten and it still
/// runs if the test panics.
pub struct EnvGuard {
    // Declared first so it is released LAST: the cwd is restored while
    // this test still holds the lock.
    _lock: MutexGuard<'static, ()>,
    prev_cwd: Option<PathBuf>,
    /// The directory-selecting variables, as they were on entry.
    /// `None` means "was unset" and is restored by REMOVING, not by
    /// setting an empty string — `config::xdg` treats empty as unset
    /// but `forced_home` filters on it too, and a test should not have
    /// to know which.
    prev_env: Vec<(&'static str, Option<String>)>,
    /// The thread-scoped directory override, when this guard installed
    /// one (`scratch_home`, `scratch_xdg`). Restored in `drop` BEFORE
    /// the lock is released. Plain `guard()` leaves it `None`.
    dirs: Option<crate::config::DirGuard>,
}

/// Every variable that decides where veles reads and writes. Snapshotted
/// and restored together: before this existed, `scratch_home` set
/// `VELES_HOME` to a temp dir and never put it back, so every test that
/// ran afterwards in the same binary inherited a pointer to a directory
/// that had already been deleted.
const DIR_VARS: &[&str] = &[
    "VELES_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "VELES_PROFILE",
    "VELES_DESKTOP_PORT",
];

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Restore the thread-scoped override FIRST, while the lock is
        // still held: a neighbour that takes the lock next must see
        // both the process env and this thread's scope cleaned up.
        self.dirs.take();
        for (k, v) in self.prev_env.drain(..) {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        if let Some(prev) = self.prev_cwd.take() {
            let _ = std::env::set_current_dir(prev);
        }
    }
}

/// Serialize AND remember the working directory, restoring it on drop.
pub fn guard() -> EnvGuard {
    let lock = lock();
    EnvGuard {
        _lock: lock,
        prev_cwd: std::env::current_dir().ok(),
        prev_env: DIR_VARS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect(),
        dirs: None,
    }
}

/// The lock, a private VELES_HOME, and cwd restoration — everything a
/// test needs before it touches process-global state.
///
/// USE THIS IN ANY TEST THAT BUILDS AN `Agent`. `Agent::new` calls
/// `MemoryStore::new().ensure_hierarchy()`, and `MemoryStore::new()`
/// reads the AMBIENT `VELES_HOME`. A test that builds an Agent without
/// setting one therefore writes a memory hierarchy into whichever
/// scratch directory some other test happens to have set — corrupting
/// its store mid-run.
///
/// That is not theoretical: it is the cause of the intermittent
/// memory:: failure that took most of a session to find. It looked like
/// a git bug (a write producing no commit, `HEAD~1` missing a file)
/// because git was what reported it, and two plausible git-side
/// theories were chased and disproved before the real answer turned up.
/// The lesson is cheap to apply and expensive to skip.
///
/// The scratch directory is published TWO ways, and the split matters:
///
///   1. A thread-scoped `config::DirOverride`. This is the deterministic
///      path: `config_dir()` on THIS thread answers from the override
///      no matter what any other thread does to the environment, so the
///      test's own reads cannot be repointed under it. It is what makes
///      `config::live()` mid-turn trustworthy.
///   2. `VELES_HOME`, under the env lock. This is the compatibility net
///      for readers that run OFF this thread and cannot see a
///      thread-local: `spawn_blocking` hook runs, `delegate` pipe
///      readers, child processes. That path is a race by nature — the
///      lock bounds it, the override does not need it.
///
/// NOTE: this TAKES the environment lock (via [`guard`]). Do NOT call
/// [`lock`] as well in the same test — `std::sync::Mutex` is not
/// reentrant, so the thread blocks on itself and the test binary hangs
/// forever with no output. That is indistinguishable from a slow build
/// and cost 40 minutes of a nix run before it was spotted.
pub fn scratch_home() -> (tempfile::TempDir, EnvGuard) {
    let mut guard = guard();
    let dir = tempfile::tempdir().expect("scratch VELES_HOME");
    register_scratch(dir.path());
    guard.dirs = Some(crate::config::set_dirs(Some(
        crate::config::DirOverride::forced(dir.path()),
    )));
    std::env::set_var("VELES_HOME", dir.path());
    (dir, guard)
}

/// A scratch environment with the XDG halves held APART: config and
/// data land in two different directories.
///
/// `scratch_home` cannot express this. A forced `VELES_HOME` collapses
/// `config_dir()`, `data_dir()` and `cache_dir()` onto one path by
/// design (the OCI image and the legacy layout both need that), so a
/// test using it can never tell which of the two a path belongs to —
/// which is precisely how five frontends came to read plugins out of
/// the data dir while the loader read the config dir, with the whole
/// suite green.
///
/// Returns (config_root, data_root, guard). The veles directories are
/// `<config_root>/veles` and `<data_root>/veles`; ask
/// `config::config_dir()` / `config::data_dir()` rather than rebuilding
/// the path, so the test breaks if the layout moves again.
pub fn scratch_xdg() -> (tempfile::TempDir, tempfile::TempDir, EnvGuard) {
    let mut guard = guard();
    let cfg = tempfile::tempdir().expect("scratch XDG_CONFIG_HOME");
    let data = tempfile::tempdir().expect("scratch XDG_DATA_HOME");
    register_scratch(cfg.path());
    register_scratch(data.path());
    // VELES_HOME wins over both, so it has to go for the split to exist.
    std::env::remove_var("VELES_HOME");
    std::env::remove_var("VELES_PROFILE");
    std::env::set_var("XDG_CONFIG_HOME", cfg.path());
    std::env::set_var("XDG_DATA_HOME", data.path());
    // The override mirrors what those vars mean on this thread, with
    // the `veles` suffix `xdg()` appplies — and pins cache beside data
    // instead of letting it fall through to the REAL `~/.cache/veles`,
    // which is where an unpinned scratch_xdg test has always quietly
    // written.
    guard.dirs = Some(crate::config::set_dirs(Some(crate::config::DirOverride {
        config: cfg.path().join("veles"),
        data: data.path().join("veles"),
        cache: data.path().join("veles-cache"),
    })));
    (cfg, data, guard)
}

/// A chat key unique to the calling test.
///
/// The ask registry (`crate::ask`) is process-global and keyed by chat,
/// and cargo runs tests in parallel threads, so two tests that both park
/// an ask under the same literal each see the other's: `open_for_chat(..)`
/// returns 2 where one test asserted 1, and a "nothing is left open"
/// assertion fails in whichever of the two finishes second. That is not
/// hypothetical — `ask_answer_is_sender_bound` and
/// `the_ask_verbs_are_bound_to_whoever_opened_the_ask` both spelled
/// `"irc:#chan"`, failed together every time they were run as a pair, and
/// failed intermittently in a full suite depending on scheduling.
///
/// libtest names a test's thread after the test, which is the one
/// per-test-unique string available without anybody remembering a
/// convention. A `#[tokio::test(flavor = "multi_thread")]` body runs on a
/// worker thread instead and would get a shared name — such a test must
/// pass its own key.
pub fn chat_key() -> String {
    let name = std::thread::current()
        .name()
        .unwrap_or("unnamed-test-thread")
        .replace("::", "_");
    format!("irc:#{name}")
}

// ── scratch directories that outlive their fixture ───────────────────

/// A scratch directory that lives until the test process exits — see
/// [`crate::scratch::kept_dir`], which other crates' tests reach
/// through the `test-support` feature.
pub fn kept_dir() -> PathBuf {
    crate::scratch::kept_dir()
}

/// The scratch-dir last line of defense (C5, the `.tmp*` leak): every
/// `scratch_home`/`scratch_xdg` root is ALSO registered here and
/// re-removed at process exit.
///
/// Why the `TempDir`'s own Drop is not enough: it removes the
/// directory WHILE `VELES_HOME` still points at it (the tuple drops
/// the dir before the `EnvGuard` restores the env), and a background
/// thread — the session heartbeat, a late store open — can recreate
/// `state.db` in that window. `remove_dir_all` then fails on the
/// recreated file, `TempDir` ignores the error (by design), and the
/// directory outlives the test. It is a race: 0, 3, or 0 leaks across
/// identical runs. At exit no test is running, so the sweep wins
/// unconditionally.
///
/// Same discipline as `scratch::kept_dir`'s atexit root: registered
/// once per process, fires on normal exit (libtest always leaves
/// through `exit`, pass or fail); a KILLED run is covered because
/// `kept_dir`'s first-call sweep and this one share `$TMPDIR`, and the
/// dirs here are tempfile's own `.tmp*` names — see `sweep_scratch`
/// for why that is safe to clean wholesale at exit.
fn register_scratch(path: &std::path::Path) {
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(|| {
        // SAFETY: a plain `extern "C" fn()` reading one static, same
        // shape as scratch.rs's atexit registration (the CRT's atexit on
        // Windows, which `process::exit` runs as well).
        unsafe {
            libc::atexit(sweep_scratch);
        }
    });
    SCRATCH_ROOTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(path.to_path_buf());
}

static SCRATCH_ROOTS: std::sync::OnceLock<std::sync::Mutex<Vec<PathBuf>>> =
    std::sync::OnceLock::new();

extern "C" fn sweep_scratch() {
    let Some(roots) = SCRATCH_ROOTS.get() else {
        return;
    };
    // One pass, then exit. A RETRY loop was tried here (measured,
    // 6 full-suite runs each way) and made leaks WORSE (15 vs 6):
    // atexit does not stop sibling threads, and the sleeping retry
    // only widens the window for a still-running background writer
    // (session heartbeat, lazy store open) to recreate what was
    // removed. Removing once and exiting immediately is the smaller
    // race. The residual (~1 dir on some runs, always containing
    // state.db/logs recreated after the test's own Drop) is the
    // process-global-singleton isolation issue recorded honestly in
    // PENDING — the sweep's job is the common case, not the last one.
    for root in roots.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        let _ = std::fs::remove_dir_all(root);
    }
}

// ── shared agent fixtures ────────────────────────────────────────────
//
// `agent::tests::agent_at_sid` was the only builder of a test Agent
// until `commands::tests` needed the same thing to prove a command can
// be answered while a turn runs. Two builders would have been two
// opinions about what a test agent is, so it moved here and agent.rs
// delegates.

/// An `Agent` pointed at `base_url`, with its own scratch store.
///
/// The caller owns the environment: take `scratch_home()` first if the
/// test touches memory, skills or anything else under VELES_HOME.
pub fn mock_agent(base_url: &str, sid: &str) -> crate::agent::Agent {
    mock_agent_with(base_url, sid, |_| {})
}

/// `mock_agent`, with a hook to adjust the config before the agent is
/// built — for tests about ROUTE SELECTION, which cannot be expressed
/// against a fixture that hard-codes one provider.
pub fn mock_agent_with(
    base_url: &str,
    sid: &str,
    tweak: impl FnOnce(&mut crate::config::VelesConfig),
) -> crate::agent::Agent {
    use std::sync::Arc;
    let dir = crate::testenv::kept_dir();
    let store = Arc::new(Mutex::new(
        crate::store::Store::open(&dir.join("state.db")).expect("store"),
    ));
    let mut built = crate::config::VelesConfig {
        model: crate::config::ModelSection {
            provider: Some("mock".into()),
            default: Some("mock-model".into()),
            ..Default::default()
        },
        custom_providers: vec![crate::config::ProviderEntry {
            name: "mock".into(),
            base_url: base_url.to_string(),
            api_mode: "chat_completions".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    tweak(&mut built);
    let cfg = Arc::new(built);
    let hooks = crate::hooks::HookBus::new(None);
    let registry = Arc::new(crate::tools::builtin_registry(store.clone(), None, sid));
    crate::agent::Agent::new(cfg, hooks, registry, store, sid.to_string()).expect("agent")
}

/// A provider that opens a stream and then STALLS until released.
///
/// The point is a turn that is genuinely, observably in flight: it has
/// taken whatever the surface holds the agent behind, it has reached
/// the socket, and it will stay there until the test decides otherwise.
/// A provider that simply never answers would do the same, but the
/// turn could then never finish and the test would leak a thread
/// wedged in `run_turn` for the life of the binary.
///
/// Returns `(base_url, release)`. Dropping or sending on `release`
/// finishes the stream.
pub fn stalling_provider() -> (String, std::sync::mpsc::Sender<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let Some(Ok(mut sock)) = listener.incoming().next() else {
            return;
        };
        // The request head is enough; the body is not interesting and
        // reading to EOF would wait for a half-close that never comes.
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf);
        let _ = sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        let _ = sock.flush();
        // Held open until released — or 20s, so a failing test still
        // terminates instead of hanging the suite.
        let _ = rx.recv_timeout(std::time::Duration::from_secs(20));
        let _ = sock.write_all(b"data: [DONE]\n\n");
        let _ = sock.flush();
    });
    (format!("http://{addr}/v1"), tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard must put the working directory back, even though the
    /// directory it was pointed at has been deleted in the meantime —
    /// that is exactly the shape of the bug it exists to prevent.
    /// (Windows refuses to delete the directory a process stands in, so
    /// there the deleted-cwd shape cannot arise; the guard still has to
    /// put the cwd back, which is what this checks on that side.)
    #[test]
    fn guard_restores_the_working_directory() {
        let before = std::env::current_dir().expect("cwd");
        let tmp = std::env::temp_dir().join(format!(
            "veles-cwd-guard-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        {
            let _g = guard();
            std::env::set_current_dir(&tmp).expect("chdir");
            // The test being modelled deletes the dir it is standing in.
            #[cfg(unix)]
            std::fs::remove_dir_all(&tmp).expect("rmdir");
        }
        #[cfg(windows)]
        std::fs::remove_dir_all(&tmp).expect("rmdir");
        let after = std::env::current_dir().expect("cwd restored");
        assert_eq!(
            after, before,
            "the guard must restore the cwd; leaving the process in a \
             deleted directory makes every later Command child fail"
        );
    }
    /// C5: the atexit sweep is what catches the racy leak — a
    /// background thread recreating `state.db` after the `TempDir`'s
    /// own Drop ran. The child leg models EXACTLY that: take a
    /// scratch_home, drop its TempDir (the normal per-test teardown),
    /// then write a file back into the (already removed) path the way
    /// a heartbeat would, and exit. Without the sweep the directory
    /// stands with that one file in it — the observed leak shape.
    ///
    /// unix only: `std::process::exit` is `ExitProcess` on Windows, which
    /// runs no CRT `atexit` handler, so there the sweep cannot fire on
    /// the way out (measured on winbox, 2026-10-13). The next run's
    /// pid-based sweep (`scratch::kept_dir`) still clears a killed run's
    /// `veles-test-<pid>` roots; a `.tmp*` root recreated after its Drop
    /// is Windows litter until a Windows exit hook exists (PLAN-windows §10).
    #[cfg(unix)]
    #[test]
    fn a_scratch_root_recreated_after_drop_is_swept_at_exit() {
        const CHILD: &str = "VELES_TESTENV_SWEEP_PROBE";
        if let Some(probe) = std::env::var_os(CHILD) {
            // Child side: normal fixture lifecycle, then the race.
            let (dir, _guard) = super::scratch_home();
            let path = dir.path().to_path_buf();
            std::fs::write(&probe, path.to_string_lossy().as_bytes()).unwrap();
            drop(dir); // the TempDir removes the directory…
            assert!(!path.exists(), "the Drop did remove it");
            // …and the heartbeat writes state.db back anyway.
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("state.db"), b"x").unwrap();
            std::process::exit(0); // atexit fires on the way out
        }
        let out = tempfile::tempdir().expect("probe dir");
        let probe = out.path().join("path.txt");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "testenv::tests::a_scratch_root_recreated_after_drop_is_swept_at_exit",
                "--exact",
                "--test-threads=1",
                "--quiet",
            ])
            .env(CHILD, &probe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run the child");
        assert!(status.success(), "child test failed: {status}");
        let leaked =
            std::path::PathBuf::from(std::fs::read_to_string(&probe).expect("child wrote"));
        assert!(
            !leaked.exists(),
            "the recreated scratch root outlived its process — the atexit sweep did not run: {}",
            leaked.display()
        );
    }
}
