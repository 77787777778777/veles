//! Wake turns — a background task's completion reaches the session even
//! when that session is IDLE.
//!
//! The USER's contract (2026-09-13): a background task's completion
//! should arrive in the session and either START an agent turn if the
//! session was idle, or arrive at the next step of the turn already
//! running. The busy half was already true — `run_turn_inner` drains
//! `delegate::take_background_events` at every round boundary, so a
//! running turn picks its completions up mid-turn. The idle half had
//! no path at all: a session with no turn running has no loop for
//! anything to arrive into.
//!
//! This module is that path, for sessions the GATEWAY or the DESKTOP
//! SERVER owns (each holds the turn-starter for its sessions for the
//! process's life):
//!
//!   1. A background task finishes and publishes `shell-complete` /
//!      `monitor-fired` / `child-complete` on the session bus —
//!      fire-and-forget, DELEGATE QUEUE FIRST: the wake loop reads
//!      `delegate::has_background_events` when it receives the bell,
//!      so the push must precede the publish or a wake turn could be
//!      admitted and drain nothing (the ordering is load-bearing).
//!   2. The owner's wake loop picks the event up, resolves the session,
//!      and — if that session has NO live turn (`session_runs` in the
//!      store: the cross-process truth, so a TUI turn answers "busy"
//!      just as loudly as a gateway one) and still has pending
//!      background events to drain — claims the wake, admits like a
//!      real inbound message (per-chat/per-session lock + rate ledger +
//!      concurrency permit where the surface has those), and runs a
//!      turn.
//!   3. The turn is `run_background_wake_turn`: the same loop, guards
//!      and verdict publication as any other; the ONLY prompt content
//!      is the drained `[BACKGROUND … COMPLETE]` rows, engine-classed.
//!      No phantom user row (the user said nothing), no `[origin]` row
//!      (nothing arrived), no `/retry` clobber (nothing to retry).
//!   4. The wake turn's boundary drains the shared queue, exactly like
//!      any turn end — a message parked while the wake ran still runs
//!      next.
//!
//! Owner-exclusivity: `own_sid` is the split. The gateway owns
//! CHAT-BACKED sessions (its meta map); the desktop server owns
//! the sessions IT holds that the gateway does not claim. A desktop
//! window may RUN a turn on a chat-backed session, but the wake for
//! one must stay the gateway's — its reply belongs in the CHAT.
//! `try_claim_wake` is the process-wide one-at-a-time gate on top:
//! whichever owner claims first runs the turn, the other stands down.
//!
//! Bounds, deliberately: (a) the wake turn itself can start background
//! work, whose completion wakes again — that is the USER's "keep
//! working" contract, and the rate ledger + concurrency permit admit
//! it exactly like any other turn; (b) the only surfaces with a
//! process-lifetime session home are the gateway and the desktop —
//! the REPL and TUI hold one session at a time and get the busy-path
//! drain and the turn-start drain, not an idle wake; (c) a session
//! whose `agent.injections.background` is muted never wakes — its
//! completions stay queued for the next real turn, exactly as today.

use std::sync::Arc;

/// What the WAKER cares about from a completion event.
#[derive(Debug, Clone)]
pub struct WakeEvent {
    pub kind: String,
    pub session_id: String,
}

/// One wake loop owner: the per-surface half of the contract.
pub trait WakeOwner: Send + Sync + 'static {
    /// Would I be the turn-starter for this session, were it idle
    /// right now and did a background task complete in it?
    fn own_sid(&self, session_id: &str) -> bool;

    /// Admit (locks, ledger, permit — silently stepped down when the
    /// session turned out busy) and run the wake turn.
    fn admit_and_run(
        &self,
        event: WakeEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// The completion kinds that may start a wake turn. Anything else on
/// the bus is someone else's business.
pub fn wake_kinds() -> &'static [&'static str] {
    &[
        "shell-complete",
        "monitor-fired",
        "child-complete",
        // A completion arrived after the running turn's last drain.
        // RunRegistration emits this once its live-run row is gone;
        // the owner waits for the surface lock and re-checks pending
        // work before making any provider call.
        "background-wake-pending",
    ]
}

/// Every wake owner running in this process (weak: an owner that goes
/// away stops counting).
fn owners() -> &'static std::sync::Mutex<Vec<std::sync::Weak<dyn WakeOwner>>> {
    static OWNERS: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Weak<dyn WakeOwner>>>> =
        std::sync::OnceLock::new();
    OWNERS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Will a background completion START a turn in this session once its
/// current turn has ended — i.e. may the model end its turn to wait?
///
/// True only when a wake owner in THIS process (the gateway, the desktop
/// server) owns the session. False in the TUI and the REPL (completions
/// wait for the next turn the human starts) and in a `veles chat -q`
/// child — every subagent — which EXITS when its turn ends. The tools
/// use this to tell the model the truth about how to wait: the store
/// showed agents pairing a background shell with a monitor and then a
/// foreground `sleep`, because nothing said ending the turn was safe.
pub fn wakes_when_idle(session_id: &str) -> bool {
    let mut owners = owners().lock().unwrap_or_else(|p| p.into_inner());
    owners.retain(|w| w.strong_count() > 0);
    owners
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .any(|o| o.own_sid(session_id))
}

/// Count `owner` in [`wakes_when_idle`] for as long as it lives.
pub(crate) fn register_owner(owner: &Arc<dyn WakeOwner>) {
    owners()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Arc::downgrade(owner));
}

/// Subscribe and run until the bus closes. Shared; the surface-specific
/// half is the [`WakeOwner`].
pub async fn run_wake_loop(owner: Arc<dyn WakeOwner>) {
    register_owner(&owner);
    let rx = crate::session_tap::subscribe();
    run_wake_loop_with(owner, rx).await;
}

/// The subscribed half, separate so a test can subscribe BEFORE driving
/// the loop. Spawning and immediately publishing has no replay buffer:
/// the event could be sent before the task's first poll subscribed.
async fn run_wake_loop_with(
    owner: Arc<dyn WakeOwner>,
    mut rx: tokio::sync::broadcast::Receiver<crate::session_tap::SessionEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if !wake_kinds().contains(&ev.kind.as_str()) {
                    continue;
                }
                let Some(sid) = ev.data.get("session_id").and_then(|s| s.as_str()) else {
                    continue;
                };
                let event = WakeEvent {
                    kind: ev.kind.clone(),
                    session_id: sid.to_string(),
                };
                if !owner.own_sid(&event.session_id) {
                    continue; // another owner's session, or nobody's
                }
                let owner = owner.clone();
                crate::rt::spawn(async move {
                    owner.admit_and_run(event).await;
                });
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Lag means those events belonged to a busy session
                // whose turn drained them at its boundary — the busy
                // path is already the truth.
                tracing::warn!(
                    "background-wake: lagged {n} event(s); turn-boundary drain covers them"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Owner {
        sid: String,
        admitted: Arc<AtomicUsize>,
    }

    impl WakeOwner for Owner {
        fn own_sid(&self, session_id: &str) -> bool {
            session_id == self.sid
        }

        fn admit_and_run(
            &self,
            _event: WakeEvent,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            self.admitted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    /// The post-turn signal travels through the REAL bus subscription
    /// and kind filter; testing only that RunRegistration published it
    /// would still pass with the wake loop refusing to listen.
    #[tokio::test]
    async fn the_wake_loop_admits_a_post_turn_completion_signal() {
        let sid = format!("wake-loop-retry-{}", uuid::Uuid::new_v4());
        let admitted = Arc::new(AtomicUsize::new(0));
        let owner: Arc<dyn WakeOwner> = Arc::new(Owner {
            sid: sid.clone(),
            admitted: admitted.clone(),
        });
        let rx = crate::session_tap::subscribe();
        let loop_task = crate::rt::spawn(run_wake_loop_with(owner, rx));
        crate::session_tap::publish(
            &sid,
            "background-wake-pending",
            serde_json::json!({ "session_id": sid }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while admitted.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the post-turn signal did not reach its owner");
        loop_task.abort();
    }
}
