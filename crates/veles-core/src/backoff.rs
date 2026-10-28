//! Per-peer retry backoff for the pool's background pushes.
//!
//! The sync loops (storesync, memsync) push to every peer on every tick.
//! A peer that refuses — its inbox full, a gap it cannot fill — or that
//! cannot be reached was asked again 30 s later with the same payload,
//! for ever: on 2026-10-02/04 the laptop re-sent 500-row chunks to a
//! wedged VPS about 1,700 times, and the phone received 960 MB over
//! mobile data in one night doing the same.
//!
//! A failure doubles the wait for that (purpose, peer) key, from twice
//! the loop's interval up to [`MAX_DELAY`]; a success clears it. State is
//! process-local: a restart forgets it, which costs one early retry.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The longest wait between two tries at one peer.
pub const MAX_DELAY: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct PeerBackoff {
    state: Mutex<HashMap<String, (u32, Instant)>>,
}

impl PeerBackoff {
    /// May `key` be tried at `now`? A key with no failures always may.
    pub fn ready(&self, key: &str, now: Instant) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .is_none_or(|(_, next)| now >= *next)
    }

    /// Record a failure at `now`; returns the wait before the next try.
    pub fn failed(&self, key: &str, base: Duration, now: Instant) -> Duration {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let failures = st.get(key).map_or(0, |(n, _)| *n).saturating_add(1);
        let factor = 1u32 << failures.min(16);
        let delay = base.saturating_mul(factor).min(MAX_DELAY);
        st.insert(key.to_string(), (failures, now + delay));
        delay
    }

    /// Record a success: the key is tried every tick again.
    pub fn succeeded(&self, key: &str) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(key);
    }

    /// Forget every key that names `peer` (the `<purpose>:<peer>` keys),
    /// e.g. when that peer is heard from again.
    pub fn reset_peer(&self, peer: &str) {
        let suffix = format!(":{peer}");
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|k, _| !k.ends_with(&suffix));
    }
}

/// The process's backoff for the pool's pushes.
pub fn pool() -> &'static PeerBackoff {
    static POOL: std::sync::OnceLock<PeerBackoff> = std::sync::OnceLock::new();
    POOL.get_or_init(PeerBackoff::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clock is passed in: the asserts are about which tries are
    /// skipped, never about time passing.
    #[test]
    fn failures_double_the_wait_up_to_the_cap_and_success_clears_it() {
        let b = PeerBackoff::default();
        let base = Duration::from_secs(30);
        let t0 = Instant::now();
        assert!(b.ready("storesync:vps", t0), "no failures: try");

        assert_eq!(b.failed("storesync:vps", base, t0), base * 2);
        assert!(
            !b.ready("storesync:vps", t0 + base),
            "the next tick is skipped"
        );
        assert!(b.ready("storesync:vps", t0 + base * 2));

        let t1 = t0 + base * 2;
        assert_eq!(b.failed("storesync:vps", base, t1), base * 4, "doubled");
        assert!(!b.ready("storesync:vps", t1 + base * 3));
        assert!(b.ready("memsync:vps", t1), "another purpose is its own key");

        for _ in 0..20 {
            b.failed("storesync:vps", base, t1);
        }
        assert_eq!(b.failed("storesync:vps", base, t1), MAX_DELAY, "capped");

        b.succeeded("storesync:vps");
        assert!(b.ready("storesync:vps", t1), "a success clears it");
    }

    #[test]
    fn hearing_from_a_peer_resets_every_purpose_for_it_only() {
        let b = PeerBackoff::default();
        let base = Duration::from_secs(30);
        let t0 = Instant::now();
        for key in ["storesync:phone", "memsync:phone", "storesync:vps"] {
            b.failed(key, base, t0);
        }
        b.reset_peer("phone");
        assert!(b.ready("storesync:phone", t0));
        assert!(b.ready("memsync:phone", t0));
        assert!(!b.ready("storesync:vps", t0), "another peer keeps its wait");
    }
}
