//! The native sandbox fence must not launch its interpreter with
//! `fork(2)`.
//!
//! This is not a style preference. veles links libtcl and keeps a Tcl
//! interpreter alive on its own thread for the life of the agent, and
//! libtcl registers a `pthread_atfork` CHILD handler
//! (`Tcl_InitNotifier` → `Tcl_GetThreadData` → `Tcl_Alloc`) that takes
//! the Tcl allocator mutex. A `fork` that lands while any other thread
//! holds that mutex leaves the child — now single-threaded — waiting on
//! a lock nobody will ever release, and leaves the PARENT waiting just
//! as permanently, inside the `recv()` std performs on the exec-status
//! socket in `Command::spawn`. That recv is a blocking syscall in the
//! middle of `poll`, so the `tokio::time::timeout` around a sandboxed
//! run cannot cancel it; the runtime thread is gone for good.
//!
//! Found 2026-09-04 when a `cargo test` run sat wedged for twelve
//! hours. `gdb` on the orphaned child showed the atfork stack, and on
//! the parent the matching `recv`. The cause was a `pre_exec` closure
//! in `apply_rlimit`, which silently disables std's `posix_spawn` fast
//! path — the fix is `mem_limited_command` in `sandbox.rs`.
//!
//! ## Why the test is shaped like this
//!
//! The bug is a RACE, and a stress test that provokes it is not a gate:
//! written first, 250 launches beside a hammering interpreter passed
//! every time on the BROKEN build, because Tcl 8.6's per-thread
//! allocator caches make the global mutex contended only rarely. A test
//! that cannot fail on the defect it names is decoration.
//!
//! So this observes the PRECONDITION instead, and does it
//! deterministically: `posix_spawn` does not run `pthread_atfork`
//! handlers and `fork` does. Register one, run the fence, and see
//! whether it fired. No timing, no luck.
//!
//! It lives in its own integration binary on purpose — the handler is
//! process-global and would also see forks from unrelated tests running
//! in parallel threads of the same binary.
#![cfg(unix)] // fork(2) and pthread_atfork are the subject; nothing to prove elsewhere

use std::sync::atomic::{AtomicI32, Ordering};

use veles_core::sandbox::{NativeBackend, SandboxBackend, SandboxRequest};

/// Write end of the report pipe, for the child handler to reach.
static REPORT_FD: AtomicI32 = AtomicI32::new(-1);

/// Runs in the CHILD after `fork(2)`, and never after `posix_spawn`.
/// `write(2)` is async-signal-safe, which is the whole budget available
/// here — the child of a fork in a threaded process may touch nothing
/// that could need a lock.
extern "C" fn note_the_fork() {
    let fd = REPORT_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = b"f";
        unsafe {
            libc::write(fd, byte.as_ptr() as *const libc::c_void, 1);
        }
    }
}

#[test]
fn the_native_fence_launches_without_forking() {
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { libc::pipe(fds.as_mut_ptr()) },
        0,
        "could not open the report pipe"
    );
    let (read_fd, write_fd) = (fds[0], fds[1]);
    REPORT_FD.store(write_fd, Ordering::Relaxed);

    assert_eq!(
        unsafe { libc::pthread_atfork(None, None, Some(note_the_fork)) },
        0,
        "could not register the atfork handler"
    );

    // A handful of launches: the observation is deterministic, so this
    // is about covering the whole path (scratch dir, driver write,
    // spawn, wait), not about rolling the dice enough times.
    let dir = std::env::temp_dir().join(format!("veles-forksafety-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let script = dir.join("user.tcl");
    std::fs::write(&script, "puts ok").expect("write the script");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    let backend = NativeBackend::new(veles_core::executecode::tclsh_path());
    let mut ran_for_real = false;
    for _ in 0..5 {
        let out = rt
            .block_on(backend.run(&SandboxRequest {
                script_path: script.clone(),
                timeout_secs: 30,
            }))
            .expect("the native backend must return, not error out");
        if out.stdout.contains("ok") {
            ran_for_real = true;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    // Close the write end so the read below cannot block: with the
    // handler's copy gone, an empty pipe reads EOF instead of waiting.
    REPORT_FD.store(-1, Ordering::Relaxed);
    unsafe { libc::close(write_fd) };
    let mut buf = [0u8; 64];
    let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    unsafe { libc::close(read_fd) };

    assert_eq!(
        n, 0,
        "the native fence forked {n} time(s) instead of using posix_spawn. \
         A fork here can deadlock permanently against libtcl's pthread_atfork \
         handler — see the module docs and `mem_limited_command` in sandbox.rs. \
         The usual cause is a `pre_exec` closure added to the sandbox Command, \
         which turns off std's posix_spawn path without saying so."
    );

    // Guard against the assertion above passing vacuously: a fence that
    // never actually ran a script would also never fork.
    assert!(
        ran_for_real,
        "no launch produced the script's output, so nothing was really spawned — \
         is tclsh on PATH?"
    );
}
