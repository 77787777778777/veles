//! The M5 owner-aware panic policy, as a PERMANENT test
//! (PLAN-plugins-installer.md M5 / R2).
//!
//! `install_fatal_panic_hook` splits on the panicking thread's OWNER and
//! the panic's LOCATION:
//!
//!   - a MODULE thread (tagged) panicking OUTSIDE core's crates → mark the
//!     instance faulted, the runtime unwinds, the process LIVES (the whole
//!     point of the module architecture: one module's bug must not take
//!     the gateway down);
//!   - a panic in core's crates (`crates/veles-{core,frontends,types}/`),
//!     or on an OWNERLESS thread → abort, so the supervisor restarts the
//!     gateway (the 2026-08-22 rationale, `lifecycle.rs`).
//!
//! The ownerless-abort branch is covered by
//! `veles_core::lifecycle::tests::fatal_panic_hook_aborts_the_process`.
//! THIS file covers the two branches a CORE test cannot reach, because
//! they need a panic located OUTSIDE core — and a panic written in a
//! veles-core test file always reports a core location:
//!
//!   1. module thread + panic outside core  → faulted, process lives;
//!   2. module thread + panic INSIDE core   → abort (the `in_core`
//!      boundary: a module that panics by way of core code is a core
//!      fault, not a module fault).
//!
//! The panic location is the file the `panic!` is written in. Branch 1
//! panics HERE (crates/veles-cli/tests/, outside core). Branch 2 drives a
//! panic from inside a veles-core fn on the tagged thread.
#![cfg(unix)] // the abort leg reads SIGABRT; the fake binaries are shell scripts

use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A 1-worker runtime whose thread is tagged `owner`, like an instance
/// runtime the module host builds (M3). Counts live threads so the test
/// can confirm the runtime is real.
fn instance_runtime(owner: &'static str) -> (tokio::runtime::Runtime, Arc<AtomicUsize>) {
    let threads = Arc::new(AtomicUsize::new(0));
    let t2 = threads.clone();
    let t3 = threads.clone();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name(format!("module:{owner}"))
        .on_thread_start(move || {
            veles_core::lifecycle::set_thread_owner(owner);
            t2.fetch_add(1, Ordering::SeqCst);
        })
        .on_thread_stop(move || {
            t3.fetch_sub(1, Ordering::SeqCst);
        })
        .enable_all()
        .build()
        .expect("instance runtime builds");
    (rt, threads)
}

/// BRANCH 1: a module-thread panic located OUTSIDE core marks the
/// instance faulted and the process LIVES.
///
/// The panic is written in THIS file (crates/veles-cli/tests/), which is
/// outside the `crates/veles-{core,frontends,types}/` list the hook's
/// `in_core` check matches. Run in-process: if the hook wrongly aborted,
/// the test binary would die by SIGABRT and the harness would report it.
#[test]
fn a_module_panic_outside_core_marks_faulted_and_the_process_lives() {
    veles_core::lifecycle::install_fatal_panic_hook();

    let before = veles_core::module::faulted_instances().len();
    let (rt, _threads) = instance_runtime("r2-branch1");

    // Panic on the tagged runtime thread, in NON-core code (this file).
    let handle = rt.spawn(async {
        panic!("R2 branch 1: a module panic outside core");
    });
    let joined = rt.block_on(handle);
    assert!(
        joined.is_err(),
        "the panicking task must propagate its panic to the runtime"
    );

    // The hook ran mark_faulted synchronously inside the panic; be
    // generous about the read.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let faulted = veles_core::module::faulted_instances();
    let owner_seen = faulted.keys().any(|k| k.contains("r2-branch1"));
    assert!(
        faulted.len() > before && owner_seen,
        "the module panic must be marked faulted (registry: {faulted:?})"
    );

    // Reaching here at all IS the assertion that the process lived: a
    // wrongly-aborting hook would have SIGABRT'd the test binary above.
    drop(rt);
}

/// BRANCH 2: a module-thread panic located INSIDE core aborts the
/// process — the `in_core` boundary. A module that panics by way of core
/// code is a core fault; the supervisor must restart the gateway.
///
/// Fork-verified (a passing abort would take the test runner with it):
/// the child tags its thread and panics from inside a veles-core fn.
#[test]
fn a_module_panic_inside_core_aborts() {
    const CHILD: &str = "VELES_TEST_M5_INCORE_CHILD";

    if std::env::var(CHILD).is_ok() {
        veles_core::lifecycle::set_thread_owner("r2-branch2");
        veles_core::lifecycle::install_fatal_panic_hook();
        // Panic from INSIDE core (the helper lives in veles-core/src/
        // module.rs, so its `panic!`'s location is a core path — the
        // `in_core` boundary the hook checks). The helper returns `!`: the
        // hook aborts the process even though this thread is tagged a
        // module's.
        veles_core::module::panic_in_core_for_m5_test();
    }

    // The abort path writes the gateway's breadcrumbs before it aborts:
    // a STOP line in gateway-shutdown.log and the pid file removed. In
    // the real home that deleted the live dev gateway's pid file on every
    // run and logged 50 false panic stops (found 2026-10-10), so the
    // child gets a home of its own.
    let home = tempfile::tempdir().expect("scratch home");
    let exe = std::env::current_exe().expect("test binary");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "a_module_panic_inside_core_aborts",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env("VELES_HOME", home.path())
        .output()
        .expect("spawn child");

    assert_eq!(
        out.status.signal(),
        Some(libc::SIGABRT),
        "a module-thread panic located INSIDE core must abort the process \
         (got {:?}); it is a core fault, and the supervisor must restart \
         the gateway.\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(home.path().join("gateway-shutdown.log")).unwrap_or_default();
    assert!(
        log.contains("panic at crates/veles-core/"),
        "the abort's STOP line is not in the scratch home: {log:?}"
    );
}
