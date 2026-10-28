//! Idle exit: `veles gateway run --exit-when-idle <secs>` ends the
//! gateway once nobody has used it for that long.
//!
//! The phone's gateway. USER 2026-10-04: "Battery usage on android is
//! horrendous, we need to make this thing yield a lot more and only
//! activate when needed." Its one job on a phone is to serve the person
//! holding it — IRC, Telegram and cron run on the VPS — so it runs while
//! it is used and stops after: no timers, no sockets, no radio. Opening
//! the app starts it again (android/java/…/GatewayService.java).
//!
//! USE is what a person does: a desktop API request (the notifier's
//! long-poll excepted — it is the app listening, not someone using it),
//! an inbound platform message, a turn in flight. Sync traffic from
//! peers is not use: they try again when the node is back.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The exit status of a gateway that stopped because it was idle, so a
/// supervisor tells it from a crash (EX_TEMPFAIL).
pub const EXIT_CODE: i32 = 75;

/// Epoch seconds of the last use; 0 = never touched (the process start
/// counts, via [`arm`]).
static LAST_USE: AtomicU64 = AtomicU64::new(0);
/// The limit in seconds; 0 = never exit on idle (the default).
static LIMIT: AtomicU64 = AtomicU64::new(0);
static EXITED: AtomicBool = AtomicBool::new(false);

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turn the idle exit on: `secs` without use and the gateway stops.
pub fn arm(secs: u64) {
    LIMIT.store(secs, Ordering::SeqCst);
    touch();
}

/// The limit in force, if the idle exit is on.
pub fn limit() -> Option<u64> {
    match LIMIT.load(Ordering::SeqCst) {
        0 => None,
        n => Some(n),
    }
}

/// Record a use now.
pub fn touch() {
    LAST_USE.store(now(), Ordering::SeqCst);
}

/// The gateway can be used from now on: the idle clock starts here, not
/// at the process start [`arm`] stamped. A boot is not idleness — on
/// 2026-10-13 a boot that took 21 s under disk load (the flake's
/// sandbox beside an image build) was over a 12 s limit before the
/// desktop API had bound, and the first idle check stopped the gateway
/// as it finished starting.
pub fn ready() {
    if let Ok(mut last) = LAST_BY.lock() {
        *last = "ready".into();
    }
    touch();
}

/// What the last use was, for the gateway's idle status line.
static LAST_BY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Record a use by `what`, and say which use ended a quiet minute: a
/// phone whose gateway never stops then has the reason in its log.
pub fn touch_by(what: impl FnOnce() -> String) {
    if limit().is_some() {
        let quiet = idle_secs();
        let what = what();
        if quiet >= 60 {
            tracing::info!("idle: used again after {quiet}s quiet — {what}");
        }
        if let Ok(mut last) = LAST_BY.lock() {
            *last = what;
        }
    }
    touch();
}

/// One line on where the idle clock stands: since the last use, what it
/// was, the turns in flight, and any background tasks still running.
pub fn status() -> String {
    let by = LAST_BY.lock().map(|s| s.clone()).unwrap_or_default();
    let live = crate::session_tap::live_sessions();
    let tasks = crate::tasks::live();
    let task_bit = if tasks.is_empty() {
        String::new()
    } else {
        format!(", {} task(s) running", tasks.len())
    };
    format!(
        "{}s since the last use ({}), limit {}s, {} turn(s) in flight{}{}",
        idle_secs(),
        if by.is_empty() {
            "the start"
        } else {
            by.as_str()
        },
        limit().unwrap_or(0),
        live.len(),
        if live.is_empty() {
            String::new()
        } else {
            format!(": {}", live.join(", "))
        },
        task_bit,
    )
}

/// Why a gateway past its idle limit is still up: the turns in flight,
/// the background tasks still running, named. None while it is not
/// past the limit (or has neither).
pub fn held_up_by() -> Option<String> {
    let l = limit()?;
    if idle_secs() < l {
        return None;
    }
    let mut reasons = Vec::new();
    let live = crate::session_tap::live_sessions();
    if !live.is_empty() {
        reasons.push(format!("turns: {}", live.join(", ")));
    }
    let tasks = crate::tasks::live();
    if !tasks.is_empty() {
        let names: Vec<String> = tasks
            .iter()
            .map(|t| format!("{}#{}", t.kind.as_str(), t.id))
            .collect();
        reasons.push(format!("tasks: {}", names.join(", ")));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

/// Seconds since the last use.
pub fn idle_secs() -> u64 {
    now().saturating_sub(LAST_USE.load(Ordering::SeqCst))
}

/// Has the gateway been idle past its limit — nobody using it, no
/// turn running, and no background work (shells, subagents, monitors)
/// still in flight? A background shell the engine started and has not
/// finished IS use: the phone's idle exit orphaned one once already
/// (2026-10-20, the guest's ulsh reparented to init, its completion
/// notice lost).
pub fn due() -> bool {
    due_given(crate::session_tap::live_sessions().is_empty() && crate::tasks::live().is_empty())
}

fn due_given(no_work_running: bool) -> bool {
    limit().is_some_and(|l| idle_secs() >= l) && no_work_running
}

/// The gateway is stopping because it was idle.
pub fn mark_exited() {
    EXITED.store(true, Ordering::SeqCst);
}

/// Did the gateway stop because it was idle?
pub fn exited() -> bool {
    EXITED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test owns the process-wide state: these are globals. The
    /// work check is passed in — the live-turn map and the task registry
    /// are every test's.
    #[test]
    fn idle_is_time_without_use_and_no_work_running() {
        assert!(!due_given(true), "off by default");
        arm(3600);
        assert!(!due_given(true), "just used");
        LAST_USE.store(now() - 7200, Ordering::SeqCst);
        assert!(
            due_given(true),
            "two hours without use, past an hour's limit"
        );
        assert!(!due_given(false), "work in flight is use");
        touch();
        assert!(!due_given(true), "a use resets the clock");
        // A boot that outlived the limit is not idleness: `ready` starts
        // the clock when the gateway can be used (2026-10-13: a 21 s boot
        // under disk load stopped a 12 s-limit gateway on its first check).
        LIMIT.store(12, Ordering::SeqCst);
        LAST_USE.store(now() - 21, Ordering::SeqCst);
        assert!(due_given(true), "sanity: 21 s past a 12 s limit is due");
        ready();
        assert!(!due_given(true), "ready starts the clock");
        assert!(status().contains("(ready)"), "{}", status());
        LIMIT.store(0, Ordering::SeqCst);
    }
}
