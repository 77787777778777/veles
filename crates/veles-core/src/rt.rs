//! Core's spawn choke point (PLAN-plugins-installer.md M3 / R2).
//!
//! Every `tokio::spawn` / `tokio::task::spawn_blocking` in `veles-core`
//! and `veles-frontends` goes through [`spawn`] / [`spawn_blocking`]
//! here, so that work started BY a module's code lands on CORE's runtime
//! and not on the module's. The plan's reason: an ask's timeout task
//! started by the IRC module must outlive the IRC module — if it spawned
//! onto IRC's instance runtime, `runtime.shutdown_timeout` at reload
//! would drop it and the ask would hang forever.
//!
//! The dispatch is on the calling thread's OWNER tag
//! ([`crate::lifecycle::thread_owner`]):
//!
//! - **A core thread** (tag `None`) behaves EXACTLY as `tokio::spawn`
//!   does today: the task attaches to the ambient runtime. No behavior
//!   change anywhere on the gateway, the desktop, the TUI, or a test.
//! - **A module thread** (tag `Some(_)`) hops onto CORE's runtime: the
//!   handle the gateway parked at boot via [`install_core_handle`]. The
//!   module's own runtime may be torn down at reload; core's is not, so
//!   core work survives it.
//!
//! The clippy `disallowed-methods` rule (`clippy.toml`) fails the gate on
//! a bare `tokio::spawn` in core/frontends, so a new site cannot bypass
//! this choke point silently.
//!
//! The hop is for CORE's futures. A module must not hand `rt::spawn` a
//! future of its own: on core's runtime it would outlive the instance
//! runtime's `shutdown_timeout`, and module code would run after the
//! module is unloaded. R3's module host keeps module code on the
//! instance runtime (and module crates get the clippy rule for this).
//!
//! What this is NOT: a place for detached work that must outlive even a
//! CALLER's runtime (the REPL's per-turn runtime). That is
//! [`crate::bg::spawn`]'s job, and its sites stay on it — a background
//! shell started mid-turn must live past the turn. `rt::spawn` is for the
//! ordinary "spawn a helper on the current runtime" sites, made safe for
//! the module case.

// This module IS the wrapper: its own `tokio::spawn` /
// `tokio::task::spawn_blocking` calls (the untagged-thread paths) are the
// legitimate final landing, so the disallowed-methods rule does not apply
// here.
#![allow(clippy::disallowed_methods)]

use std::sync::OnceLock;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// Core's runtime handle, parked by the gateway at boot
/// ([`install_core_handle`]). `None` means "no core runtime was parked":
/// a REPL/TUI/test process that never called the installer. In that case
/// a module-thread spawn cannot hop anywhere better, so it falls back to
/// the ambient runtime if there is one, else to [`crate::bg::spawn`]'s
/// detached runtime — the same safe landing a bare spawn would have had.
static CORE_HANDLE: OnceLock<Handle> = OnceLock::new();

/// Park core's runtime handle. The gateway calls this ONCE, right after
/// building its runtime, BEFORE any module can spawn. Idempotent
/// (a second call in a test boot is ignored; the first handle wins, which
/// is correct: it is the long-lived gateway runtime).
pub fn install_core_handle(handle: Handle) {
    let _ = CORE_HANDLE.set(handle);
}

/// The parked core handle, if the gateway installed one.
fn core_handle() -> Option<&'static Handle> {
    CORE_HANDLE.get()
}

/// Spawn `fut` the way core code should (M3): onto CORE's runtime when
/// called from a module thread, onto the ambient runtime otherwise.
///
/// See the module docs for the dispatch. The future and its output must
/// be `Send + 'static`, exactly as for `tokio::spawn`.
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    // A MODULE thread hops to core's runtime so the task outlives the
    // module's instance runtime. An untagged (core) thread keeps today's
    // behavior — the ambient runtime — so nothing else changes.
    if crate::lifecycle::thread_owner().is_some() {
        if let Some(h) = core_handle() {
            return h.spawn(fut);
        }
        // No parked core handle (a test or a non-gateway process): land
        // on the ambient runtime if there is one, else on bg's detached
        // runtime so the spawn can never panic where `tokio::spawn` would.
        if let Ok(h) = Handle::try_current() {
            return h.spawn(fut);
        }
        return crate::bg::spawn(fut);
    }
    tokio::spawn(fut)
}

/// Spawn blocking work the way core code should (M3), same dispatch as
/// [`spawn`]: core's runtime from a module thread, the ambient runtime
/// otherwise. `spawn_blocking` is how sync code (a Tcl eval, a store
/// borrow, a TLS handshake) is driven off the async workers; on a module
/// thread that pool must be CORE's, or a reload's `shutdown_timeout`
/// would cancel the blocking call mid-flight.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    if crate::lifecycle::thread_owner().is_some() {
        if let Some(h) = core_handle() {
            return h.spawn_blocking(f);
        }
        if let Ok(h) = Handle::try_current() {
            return h.spawn_blocking(f);
        }
        return crate::bg::handle().spawn_blocking(f);
    }
    tokio::task::spawn_blocking(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Scope an owner tag for the test, restoring the previous tag after
    /// (the production tag is set once per thread and never cleared; the
    /// test must not leak one case's module identity into the next).
    fn tagged<R>(owner: Option<&'static str>, f: impl FnOnce() -> R) -> R {
        let prev = crate::lifecycle::thread_owner();
        if let Some(o) = owner {
            crate::lifecycle::set_thread_owner(o);
        }
        let r = f();
        crate::lifecycle::clear_thread_owner_for_test(prev);
        r
    }

    /// On an untagged (core) thread, `rt::spawn` behaves exactly as
    /// `tokio::spawn`: the task runs on the ambient runtime.
    #[tokio::test]
    async fn a_core_thread_spawn_lands_on_the_ambient_runtime() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        spawn(async move {
            flag.store(true, Ordering::SeqCst);
        })
        .await
        .expect("the spawn joined");
        assert!(done.load(Ordering::SeqCst));
    }

    /// THE point of the module (plan R2 test 3): a spawn made from a
    /// MODULE thread runs on CORE's runtime, not the module's — so it
    /// survives the module runtime's `shutdown_timeout`.
    #[test]
    fn a_module_thread_spawn_hops_to_cores_runtime() {
        // Core's runtime: parked as the gateway would park it, with a
        // NAMED worker so the task's landing thread identifies it.
        let core_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("veles-test-core")
            .enable_all()
            .build()
            .expect("core runtime");
        install_core_handle(core_rt.handle().clone());

        // The module's runtime: tagged, 1 worker, its OWN name.
        let module_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("veles-test-module")
            .enable_all()
            .build()
            .expect("module runtime");

        let ran_thread = Arc::new(std::sync::Mutex::<Option<String>>::new(None));
        let rt_name = ran_thread.clone();

        // From a tagged thread, spawn via rt::spawn and record WHICH
        // thread the task actually ran on.
        tagged(Some("module:echo"), || {
            spawn(async move {
                let on = std::thread::current().name().unwrap_or("").to_string();
                *rt_name.lock().unwrap() = Some(on);
            });
        });

        // Drive core's runtime so the task runs (the module runtime is
        // never driven: if the task landed on it, it would NOT run here).
        core_rt.block_on(async {
            for _ in 0..50 {
                if ran_thread.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        let landed = ran_thread.lock().unwrap().clone();
        assert_eq!(
            landed.as_deref(),
            Some("veles-test-core"),
            "the task must run on core's runtime (landed on {landed:?})"
        );

        // And it must survive the module runtime going away: drop it, then
        // spawn again from a tagged thread and drive core — still runs.
        drop(module_rt);
        let after = Arc::new(AtomicBool::new(false));
        let f2 = after.clone();
        tagged(Some("module:echo"), || {
            spawn(async move {
                f2.store(true, Ordering::SeqCst);
            });
        });
        core_rt.block_on(async {
            for _ in 0..50 {
                if after.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        assert!(
            after.load(Ordering::SeqCst),
            "core work survives the module runtime's teardown"
        );
    }

    /// A module-thread spawn with NO parked core handle and NO ambient
    /// runtime still lands somewhere (bg's detached runtime) rather than
    /// panicking — the safe landing a non-gateway process needs.
    #[test]
    fn a_module_thread_spawn_with_no_core_handle_still_runs() {
        // Do NOT install a core handle in this leg: run in a subprocess so
        // the OnceLock is fresh (the previous test installed one).
        const CHILD: &str = "VELES_TEST_RT_NOHANDLE_CHILD";
        if std::env::var(CHILD).is_err() {
            let exe = std::env::current_exe().expect("test binary");
            let out = std::process::Command::new(exe)
                .args([
                    "--exact",
                    "rt::tests::a_module_thread_spawn_with_no_core_handle_still_runs",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .output()
                .expect("spawn child");
            assert!(
                out.status.success(),
                "child failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }

        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        std::thread::spawn(move || {
            tagged(Some("module:echo"), || {
                assert!(Handle::try_current().is_err(), "no ambient runtime");
                spawn(async move {
                    flag.store(true, Ordering::SeqCst);
                });
            });
            // Drive the detached bg runtime indirectly: the task runs on
            // its own, so just wait for the flag.
            for _ in 0..100 {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("the spawn never ran on the detached runtime");
        })
        .join()
        .expect("the test thread did not panic");
    }
}
