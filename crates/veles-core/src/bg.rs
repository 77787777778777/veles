//! A process-wide runtime for DETACHED background work.
//!
//! WHY THIS EXISTS. Three kinds of work are spawned and then left to
//! run on their own — a background shell (`shells::start`), a delegated
//! subagent (`delegate::spawn_background`), and a monitor
//! (`monitor::start`). Each used to call `tokio::spawn`, which attaches
//! the task to whatever runtime happens to be current for the CALLER.
//! That is correct on the gateway and the desktop, which both run one
//! long-lived runtime for the life of the process. It is a bug on the
//! owner surfaces:
//!
//!   - The REPL and the TUI build a runtime INSIDE `run_turn_on_thread`
//!     and drop it the moment the turn returns. A `terminal
//!     background=true` shell, or a `delegate_task background=true`
//!     child, spawned during a turn therefore had its runtime pulled
//!     out from under it at the turn boundary — its
//!     [BACKGROUND … COMPLETE] injection could never arrive, because
//!     the task driving it no longer had anywhere to run.
//!   - The slash-command dispatcher is SYNCHRONOUS on those surfaces
//!     (it is reached from the readline loop, not from inside a
//!     runtime). `/background <cmd>` calling `tokio::spawn` there
//!     panicked outright: "there is no reactor running" — observed live
//!     before this module existed.
//!
//! The fix is one runtime that outlives every caller. Detached work is
//! spawned onto it regardless of which surface asked, so a background
//! shell started from the REPL's `/background`, or from a `terminal`
//! call mid-turn, keeps running after the turn that started it has
//! ended. It is `multi_thread` + `enable_all` because these tasks sleep
//! (monitor poll loops) and do async IO (the delegate child, the shell
//! reader tasks); a current-thread runtime would serialize them behind
//! one blocking call.
//!
//! Inner spawns are deliberately left on plain `tokio::spawn`: once a
//! task is running on THIS runtime, `tokio::spawn` inside it attaches to
//! this same runtime, so the shell's stdout/stderr reader tasks inherit
//! it for free. Only the OUTER spawn of each detached unit needs to go
//! through here.
//!
//! Two worker threads, matching the bridge's runtime: background work
//! is latency-insensitive (nobody is blocked on it — that is the whole
//! point), so two threads let a slow shell and a slow subagent overlap
//! without giving detached work a large standing thread pool.

// bg is the detached-work runtime: its own `bg_runtime().spawn` /
// `.spawn_blocking` calls are the legitimate landing for work that must
// outlive even a caller's runtime, and rt.rs routes its no-runtime
// fallback here. The disallowed-methods rule targets bare `tokio::spawn`
// in ORDINARY code; bg is the deliberate exception.
#![allow(clippy::disallowed_methods)]

use std::sync::OnceLock;
use tokio::runtime::{Handle, Runtime};

static BG: OnceLock<Runtime> = OnceLock::new();

/// The lazily-built background runtime. Constructing a `Runtime` does
/// not enter or block it, so this is safe to call from inside another
/// runtime (the gateway) — only `block_on` is forbidden nested, and
/// this never blocks.
fn bg_runtime() -> &'static Runtime {
    BG.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("veles-bg")
            .build()
            .expect("the background runtime must build: it is the only one detached work has")
    })
}

/// Spawn detached work that must outlive its caller. Safe from BOTH a
/// synchronous context (the REPL/TUI slash dispatcher) and an async one
/// (the gateway, the desktop API): it never relies on an ambient
/// runtime, so it cannot panic where `tokio::spawn` would.
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    bg_runtime().spawn(fut)
}

/// A handle to the background runtime, for a caller that needs to
/// `spawn_blocking` onto the same runtime its detached tasks live on.
pub fn handle() -> Handle {
    bg_runtime().handle().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// The whole reason this module exists: a spawn from a thread with
    /// NO ambient runtime must succeed rather than panic. `tokio::spawn`
    /// here would abort with "there is no reactor running".
    #[test]
    fn spawns_from_a_thread_with_no_runtime() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        std::thread::spawn(move || {
            // No runtime entered on this thread.
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "the test thread must have no ambient runtime for this to mean anything"
            );
            let jh = spawn(async move {
                flag.store(true, Ordering::SeqCst);
            });
            // Block on it from a throwaway runtime to prove the task ran
            // on the BACKGROUND one, not on a caller runtime.
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(jh).unwrap();
        })
        .join()
        .unwrap();
        assert!(done.load(Ordering::SeqCst), "the detached task ran");
    }

    /// The REPL/TUI shape: a runtime that is built, used to spawn, and
    /// DROPPED — the work must still complete, because it is on the
    /// background runtime and not the dropped one. (Spawning here from
    /// inside `block_on` is the hard case: the ambient runtime exists at
    /// spawn time and is gone at await time.)
    #[test]
    fn detached_work_survives_its_caller_runtime_being_dropped() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        // The handle travels out through a slot rather than as the
        // async block's value: `block_on` must return `()`, and the
        // handle has to be awaited on a DIFFERENT runtime than the one
        // that was alive when it was spawned — that difference is the
        // property under test.
        let slot: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> =
            std::sync::Mutex::new(None);
        {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // Spawned with an ambient runtime present — the case
                // where a plain `tokio::spawn` would silently attach to
                // `rt` and die with it.
                let h = spawn(async move { flag.store(true, Ordering::SeqCst) });
                *slot.lock().unwrap() = Some(h);
            });
            // rt dropped here, with the work still owing its completion
            // to whichever runtime actually owns it.
        }
        let jh = slot.into_inner().unwrap().expect("the block spawned");
        // Await it on a fresh runtime: the task lives on bg, not on the
        // dropped one, so the handle is still good.
        let rt2 = tokio::runtime::Runtime::new().unwrap();
        rt2.block_on(jh).unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    /// The handle is the background runtime's, so a caller can put
    /// blocking work on the same runtime its detached tasks use.
    #[test]
    fn handle_drives_spawn_blocking_on_the_same_runtime() {
        let out =
            handle().block_on(async { tokio::task::spawn_blocking(|| 21 * 2).await.unwrap() });
        assert_eq!(out, 42);
    }
}
