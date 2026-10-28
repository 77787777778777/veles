//! Scratch directories for TESTS that must outlive their fixture.
//!
//! Compiled for this crate's own tests and, through the `test-support`
//! feature, for other crates' tests (veles-irc parks process-global
//! stores the same way). Nothing in a production path calls it.

use std::path::PathBuf;

/// A scratch directory that lives until the TEST PROCESS exits.
///
/// For fixtures that hand out something holding a file open — an
/// `Agent` or a `ToolRegistry` over a sqlite store — and cannot also
/// hand back the `TempDir` that owns it. They used to
/// `std::mem::forget` the `TempDir`, which kept the store alive and
/// never deleted anything: /dev/shm held 5,405 leaked `.tmp*`
/// directories (1.47 GB of RAM) after three days of test runs.
///
/// Every such directory sits under one per-process root,
/// `$TMPDIR/veles-test-<pid>/`, which an `atexit` handler removes when
/// the test binary exits (pass or fail — libtest leaves through
/// `exit`). A run that is KILLED never reaches `atexit`, and the
/// mutation harnesses kill runs routinely, so the first call in every
/// process also sweeps roots whose pid is gone.
pub fn kept_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let root = KEPT_ROOT.get_or_init(|| {
        let base = std::env::temp_dir();
        sweep_dead_kept_roots(&base);
        let root = base.join(format!("veles-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("kept_dir root");
        // SAFETY: registers a plain `extern "C" fn()` with no captured
        // state; it only reads a OnceLock that is set before this call.
        // libc's atexit is the CRT's on Windows too, and `process::exit`
        // runs it there as well.
        unsafe {
            libc::atexit(remove_kept_root);
        }
        root
    });
    let dir = root.join(N.fetch_add(1, Ordering::Relaxed).to_string());
    std::fs::create_dir_all(&dir).expect("kept_dir");
    dir
}

static KEPT_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

extern "C" fn remove_kept_root() {
    if let Some(root) = KEPT_ROOT.get() {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Remove `veles-test-<pid>` roots under `base` whose process no longer
/// exists — what a killed run left behind. A live pid (ours, a
/// neighbouring test binary's) is never touched.
fn sweep_dead_kept_roots(base: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("veles-test-"))
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        if !pid_alive(pid) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Does the process still exist? The platform layer's answer (`kill(pid,
/// 0)` on unix, `OpenProcess` + the exit code on Windows); a pid that
/// cannot be a pid is "alive", so nothing doubtful is swept.
fn pid_alive(pid: i32) -> bool {
    u32::try_from(pid).is_ok_and(crate::platform::pid_alive)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A killed run's root (its pid is gone) is swept; a live process's
    /// root — ours here — is left alone.
    #[cfg(unix)]
    #[test]
    fn a_dead_runs_root_is_swept_and_a_live_one_kept() {
        let base = tempfile::tempdir().expect("base");
        // A pid that existed and is now gone: spawn, reap, reuse its id.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let dead = child.id();
        child.wait().expect("reap");
        let dead_root = base.path().join(format!("veles-test-{dead}"));
        let live_root = base
            .path()
            .join(format!("veles-test-{}", std::process::id()));
        let unrelated = base.path().join("veles-test-notapid");
        for d in [&dead_root, &live_root, &unrelated] {
            std::fs::create_dir_all(d.join("0")).unwrap();
        }
        sweep_dead_kept_roots(base.path());
        assert!(!dead_root.exists(), "a dead run's root must be swept");
        assert!(live_root.exists(), "a live process's root must survive");
        assert!(unrelated.exists(), "only veles-test-<pid> names are ours");
    }

    /// The whole point: a test process that used `kept_dir` leaves
    /// nothing behind once it exits. Runs this binary again as a child
    /// that makes a kept dir and reports its path, then checks the path
    /// is gone after the child exited.
    #[test]
    fn a_processs_kept_dirs_are_gone_after_it_exits() {
        if let Some(probe) = std::env::var_os("VELES_KEPT_DIR_PROBE") {
            // Child side.
            let d = kept_dir();
            std::fs::write(d.join("state.db"), b"x").unwrap();
            std::fs::write(probe, d.to_string_lossy().as_bytes()).unwrap();
            return;
        }
        let out = tempfile::tempdir().expect("probe dir");
        let probe = out.path().join("path.txt");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "scratch::tests::a_processs_kept_dirs_are_gone_after_it_exits",
                "--exact",
                "--test-threads=1",
                "--quiet",
            ])
            .env("VELES_KEPT_DIR_PROBE", &probe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run the child");
        assert!(status.success(), "child test failed: {status}");
        let kept = std::path::PathBuf::from(std::fs::read_to_string(&probe).expect("child wrote"));
        assert!(
            kept.starts_with(std::env::temp_dir()),
            "kept dirs live under TMPDIR: {}",
            kept.display()
        );
        assert!(
            !kept.exists(),
            "the kept dir outlived its process: {}",
            kept.display()
        );
        assert!(
            !kept.parent().unwrap().exists(),
            "and so did its per-process root"
        );
    }
}
