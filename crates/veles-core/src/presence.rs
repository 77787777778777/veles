//! Which sessions THIS process holds open, and how to wake each one
//! (PLAN-sessions-and-phone.md S2).
//!
//! `session_runs` answers "who is running a turn"; nothing answered
//! "who has this session open and would run a turn for it if asked". A
//! surface that holds sessions — the gateway's chats, the desktop
//! server's cached agents, a TUI, a REPL — registers a
//! [`PresenceSource`] here. [`crate::runsock`] serves the union to
//! sibling processes, and [`crate::mail`] nudges through it.
//!
//! Owner-tracked like the session runners (`crate::cron`): a module
//! surface's source is purged with the module.

use std::sync::Arc;

/// One session a process holds open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSession {
    pub session_id: String,
    /// The surface holding it: `desktop`, `tui`, `repl`, `telegram`, …
    pub surface: String,
}

/// A surface that holds sessions.
pub trait PresenceSource: Send + Sync + 'static {
    /// The sessions this surface holds right now.
    fn open_sessions(&self) -> Vec<OpenSession>;

    /// Something is waiting in this session's queue. When the session
    /// is mine: run it now if idle, or leave it to my turn boundary if
    /// busy, and answer `true`. When it is not mine: `false`, and do
    /// nothing. Must not block — spawn or post, never run the turn on
    /// the caller's thread.
    fn nudge(&self, session_id: &str) -> bool;
}

static SOURCES: std::sync::OnceLock<crate::owned::OwnedRegistry<u64, Arc<dyn PresenceSource>>> =
    std::sync::OnceLock::new();
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn sources() -> &'static crate::owned::OwnedRegistry<u64, Arc<dyn PresenceSource>> {
    SOURCES.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Register a surface. Returns the key [`unregister`] takes. The owner
/// is read from the calling thread (M4).
pub fn register(src: Arc<dyn PresenceSource>) -> u64 {
    let k = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    sources().insert(k, src);
    k
}

/// Remove one registration (a TUI that switched off, a test).
pub fn unregister(key: u64) {
    sources().remove(&key);
}

/// Remove every source `owner` registered (R3 teardown).
pub fn purge_owner(owner: Option<&'static str>) -> usize {
    sources().purge(owner)
}

/// Every session this process holds, each once (the first source to
/// name a session wins its surface).
pub fn list() -> Vec<OpenSession> {
    let mut out: Vec<OpenSession> = Vec::new();
    for src in sources().values_cloned() {
        for s in src.open_sessions() {
            if !out.iter().any(|o| o.session_id == s.session_id) {
                out.push(s);
            }
        }
    }
    out
}

/// Does this process hold the session?
pub fn holds(session_id: &str) -> bool {
    list().iter().any(|s| s.session_id == session_id)
}

/// Nudge the session here. `true` = a source here holds it and took the
/// nudge. Sources are asked in turn until one answers for it.
pub fn nudge(session_id: &str) -> bool {
    // Collected first: a source's nudge may register or unregister (a
    // TUI switching sessions), and the registry must not be held then.
    let srcs = sources().values_cloned();
    srcs.iter().any(|s| s.nudge(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        sid: &'static str,
        nudged: Mutex<Vec<String>>,
    }

    impl PresenceSource for Fake {
        fn open_sessions(&self) -> Vec<OpenSession> {
            vec![OpenSession {
                session_id: self.sid.into(),
                surface: "fake".into(),
            }]
        }
        fn nudge(&self, sid: &str) -> bool {
            if sid == self.sid {
                self.nudged.lock().unwrap().push(sid.into());
                true
            } else {
                false
            }
        }
    }

    #[test]
    fn a_nudge_reaches_the_source_that_holds_the_session_and_no_other() {
        let a = Arc::new(Fake {
            sid: "presence-a",
            nudged: Mutex::new(vec![]),
        });
        let b = Arc::new(Fake {
            sid: "presence-b",
            nudged: Mutex::new(vec![]),
        });
        let ka = register(a.clone());
        let kb = register(b.clone());
        assert!(nudge("presence-b"));
        assert!(!nudge("presence-nobody"));
        assert!(a.nudged.lock().unwrap().is_empty());
        assert_eq!(*b.nudged.lock().unwrap(), vec!["presence-b".to_string()]);
        assert!(holds("presence-a") && holds("presence-b"));
        unregister(ka);
        unregister(kb);
        assert!(!holds("presence-a"));
    }
}
