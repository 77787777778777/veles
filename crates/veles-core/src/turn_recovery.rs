//! Restart recovery of an ordinary local desktop turn.
//!
//! The run generation claims ownership. The checkpoint and saved message
//! IDs decide what is safe to do: finish a saved reply, continue after a
//! saved tool result, or stop for review when an action's result is unknown.
//! Connector and A2A turns need their original principal and delivery route
//! and are deliberately outside this owner path, as are session-mail turns
//! (surface "mail": they ran under the mail's capped rights), cron,
//! subagent, TUI, app and trigger turns.
//!
//! Hosts without a process lifetime identity (`platform::process_start_id`
//! is `None`: macOS and the BSDs today) record no owner start, and their
//! checkpoints are never candidates: recovery is off there rather than
//! guessing whether a recycled PID is the owner.

use crate::store::{InterruptedRun, RecoveryClaim, Store};
use anyhow::{Context, Result};

/// The surface whose interrupted turns this module may resume.
pub const RESUMABLE_SURFACE: &str = "desktop";

pub fn candidates(store: &Store) -> Result<Vec<InterruptedRun>> {
    Ok(store
        .recovery_candidates()?
        .into_iter()
        .filter(|run| run.surface == RESUMABLE_SURFACE && !run.turn_id.is_empty())
        .collect())
}

/// Close the checkpoints of dead owners that nothing will ever resume (a
/// connector, mail, subagent or cron turn), so they stop reading as
/// "running" forever. Same-host, dead-owner, unclaimed rows only: a live
/// turn on any surface, and another host's, are left alone.
pub fn retire_unresumable(store: &Store) -> Result<usize> {
    let mut closed = 0;
    for run in store.recovery_candidates()? {
        if run.surface != RESUMABLE_SURFACE {
            store.turn_checkpoint_finish(&run.turn_id, "abandoned", None)?;
            closed += 1;
        }
    }
    Ok(closed)
}

pub fn claim(store: &Store, run: &InterruptedRun) -> Result<Option<RecoveryClaim>> {
    store
        .recovery_claim(run, std::process::id(), &run.surface)
        .with_context(|| format!("claim interrupted session {}", run.session_id))
}

pub fn interruption_note(_run: &InterruptedRun) -> String {
    "The gateway restarted while this turn was running. Continuing from the saved conversation and tool results; no new user prompt was sent. An external action whose result was not saved may need review.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ChatMessage;

    fn dead_process() -> (u32, String) {
        crate::platform::exited_process_for_test()
    }

    fn orphan(store: &Store, session: &str) {
        orphan_on(store, session, "desktop");
    }

    fn orphan_on(store: &Store, session: &str, surface: &str) {
        store.create_session(session).unwrap();
        store
            .append_message(session, &ChatMessage::user("original"))
            .unwrap();
        let start = store
            .load_history(session)
            .unwrap()
            .last()
            .unwrap()
            .msg_id
            .unwrap();
        let _ = start;
        let (pid, process_start) = dead_process();
        store
            .begin_turn_fenced(session, pid, &crate::platform::hostname(), surface)
            .unwrap();
        store.conn().execute(
            "UPDATE turn_checkpoints SET owner_pid = ?2, owner_start = ?3 WHERE session_id = ?1",
            rusqlite::params![session, pid, process_start],
        ).unwrap();
    }

    #[test]
    fn candidates_exclude_live_other_host_and_unregistered_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        orphan(&store, "orphan");
        store.create_session("live").unwrap();
        store
            .begin_turn_fenced(
                "live",
                std::process::id(),
                &crate::platform::hostname(),
                "desktop",
            )
            .unwrap();
        store.create_session("elsewhere").unwrap();
        store
            .begin_turn_fenced("elsewhere", u32::MAX - 1, "other-host", "desktop")
            .unwrap();
        let found = candidates(&store).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "orphan");
    }

    #[test]
    fn a_reaped_liveness_row_does_not_lose_the_interrupted_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        orphan(&store, "orphan");
        store.run_end("orphan").unwrap();
        let found = candidates(&store).unwrap();
        assert_eq!(found.len(), 1);
        let claimed = claim(&store, &found[0]).unwrap().expect("checkpoint claim");
        store.recovery_release(&claimed).unwrap();
        assert!(store.runs_current(30).unwrap().is_empty());
    }

    #[test]
    fn claim_is_generation_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        orphan(&store, "orphan");
        let run = candidates(&store).unwrap().remove(0);
        let claimed = claim(&store, &run).unwrap().expect("orphan");
        assert_eq!(claimed.turn_id, run.turn_id);
        assert!(claim(&store, &run).unwrap().is_none());
        store.recovery_release(&claimed).unwrap();
        assert!(store.runs_current(30).unwrap().is_empty());
    }

    /// Every claim counts, across releases and processes: the bound that
    /// stops a turn from being paid for on every boot.
    #[test]
    fn each_claim_is_a_numbered_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        orphan(&store, "orphan");
        for expected in 1..=3 {
            let run = candidates(&store).unwrap().remove(0);
            let claimed = claim(&store, &run).unwrap().expect("released claim");
            assert_eq!(claimed.attempt, expected);
            store.recovery_release(&claimed).unwrap();
        }
    }

    /// Dead owners' checkpoints that no recovery path resumes are closed
    /// at startup; a live turn and a resumable desktop turn are not.
    #[test]
    fn unresumable_orphans_are_retired_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        orphan(&store, "desk");
        orphan_on(&store, "sub", "other");
        orphan_on(&store, "chan", "irc");
        orphan_on(&store, "mailed", "mail");
        store.create_session("live-irc").unwrap();
        store
            .begin_turn_fenced(
                "live-irc",
                std::process::id(),
                &crate::platform::hostname(),
                "irc",
            )
            .unwrap();
        assert_eq!(retire_unresumable(&store).unwrap(), 3);
        let state = |sid: &str| store.turn_checkpoint(sid).unwrap().unwrap().state;
        assert_eq!(state("sub"), "abandoned");
        assert_eq!(state("chan"), "abandoned");
        assert_eq!(state("mailed"), "abandoned");
        assert_eq!(state("desk"), "running");
        assert_eq!(state("live-irc"), "running");
        assert_eq!(candidates(&store).unwrap().len(), 1);
    }

    /// A process that has exited is dead even while something still holds
    /// it (an unreaped child, a held Windows handle): the owner identity
    /// must not outlive the process.
    #[test]
    fn an_exited_owner_has_no_lifetime_identity() {
        let mut child = crate::platform::stdin_waiting_child_for_test();
        let pid = child.id();
        assert!(crate::platform::process_start_id(pid).is_some());
        child.kill().unwrap();
        // Not reaped yet: a zombie on unix, a held handle on Windows.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(crate::platform::process_start_id(pid), None);
        child.wait().unwrap();
    }
}
