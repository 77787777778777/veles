//! Monitors — wait for a condition, then wake the conversation.
//!
//! WHAT WAS HERE BEFORE: nothing. The 2026-09-02 audit grepped the whole
//! tree for a wait/watch/condition-poll mechanism and found only display
//! monitors in the Tk desktop and the vendored IRCv3 MONITOR capability.
//! This is a new subsystem, built because polling a background shell in
//! a tool loop burns a round trip per check and the model has no way to
//! sleep.
//!
//! THE SHAPE IS `ask.rs`. That module already solves "register pending
//! work under an id, resolve it later, time it out, cancel it, and
//! enumerate what is open". A monitor is the same thing with a predicate
//! instead of a human on the other end, so the structure is deliberately
//! parallel rather than novel.
//!
//! THE WAKE CONTRACT, stated plainly because it is easy to oversell this:
//! a monitor publishes a durable background event and a session-bus signal.
//! The gateway or desktop owning that session starts a wake turn while it is
//! idle; an already-running turn drains the event at its boundary instead.
//! `RunRegistration` rings once more after a turn ends if work arrived after
//! its final drain, so the final-response/teardown handoff is covered too.
//! This starts a new turn rather than interrupting a running provider call.
//! An unheld session has no surface that can run that turn: its durable event
//! remains parked for the next surface that owns it. Background injection
//! must also be enabled; disabling it is an explicit opt-out of wake turns.

use crate::tasks::{TaskKind, TaskStatus};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// How often a predicate is evaluated.
const POLL_SECS: u64 = 2;
/// Ceiling on any monitor's lifetime, so a never-true predicate cannot
/// leave a polling task running for the life of the process.
const MAX_DEADLINE_SECS: u64 = 3_600;
const DEFAULT_DEADLINE_SECS: u64 = 600;

/// What a monitor is waiting for.
#[derive(Debug, Clone)]
pub enum Predicate {
    /// A shell command exits 0.
    Shell(String),
    /// Another task reaches a terminal state.
    Task(u32),
    /// A path exists (and, if it already did, that its mtime moves).
    File(String),
}

impl Predicate {
    pub fn describe(&self) -> String {
        match self {
            Predicate::Shell(c) => format!("shell: {c}"),
            Predicate::Task(id) => format!("task {id} finishes"),
            Predicate::File(p) => format!("file: {p}"),
        }
    }

    /// Parse from tool arguments. Exactly one of the three keys.
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let shell = args["shell"].as_str().filter(|s| !s.trim().is_empty());
        let task = args["task"].as_u64();
        let file = args["file"].as_str().filter(|s| !s.trim().is_empty());
        match (shell, task, file) {
            (Some(c), None, None) => Ok(Predicate::Shell(c.to_string())),
            (None, Some(t), None) => Ok(Predicate::Task(t as u32)),
            (None, None, Some(p)) => Ok(Predicate::File(p.to_string())),
            (None, None, None) => Err("one of 'shell', 'task' or 'file' is required".to_string()),
            _ => Err("give exactly one of 'shell', 'task' or 'file'".to_string()),
        }
    }
}

/// Where a monitor's predicate is evaluated. A monitor is a `shell`
/// command and a `stat` by another name, so it belongs in the same
/// fence as `terminal` and `read_file` — before 2026-09-09 it was
/// unclassified AND unrouted, which made it the way around both.
#[derive(Debug, Clone)]
enum Fence {
    Host,
    /// The caller's sandbox, plus the container-side mtime baseline for
    /// a `file` predicate (unix seconds; None = the path was absent).
    Sandbox {
        key: String,
        baseline: Option<i64>,
    },
}

/// The container-side mtime of `path`, or None when it does not exist.
async fn env_mtime(key: &str, path: &str) -> Option<i64> {
    let target = crate::user_envs::container_resolve(path);
    let out = crate::user_envs::exec_argv(
        key,
        &[
            "stat".to_string(),
            "-c".to_string(),
            "%Y".to_string(),
            target,
        ],
        POLL_SECS.max(5),
    )
    .await
    .ok()?;
    if out.exit_code != 0 {
        return None;
    }
    out.stdout.trim().parse::<i64>().ok()
}

/// Evaluate once. `true` means the condition holds.
async fn holds(p: &Predicate, baseline: Option<std::time::SystemTime>, fence: &Fence) -> bool {
    match (p, fence) {
        // ── sandbox zone: the predicate runs inside the container ──
        (Predicate::Shell(cmd), Fence::Sandbox { key, .. }) => {
            crate::user_envs::exec_shell(key, cmd, POLL_SECS.max(10))
                .await
                .map(|o| o.exit_code == 0)
                .unwrap_or(false)
        }
        (
            Predicate::File(path),
            Fence::Sandbox {
                key,
                baseline: base,
            },
        ) => match env_mtime(key, path).await {
            None => false,
            Some(now) => match base {
                None => true,
                Some(b) => now > *b,
            },
        },
        // ── host zone: unchanged ──
        (Predicate::Shell(cmd), Fence::Host) => crate::shell::command(cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false),
        (Predicate::File(path), Fence::Host) => match std::fs::metadata(path) {
            Err(_) => false,
            Ok(m) => match baseline {
                // Did not exist when the monitor was created: existing
                // now is the event.
                None => true,
                // Existed already: wait for the mtime to move.
                Some(b) => m.modified().map(|t| t > b).unwrap_or(false),
            },
        },
        // A task id is process state, not filesystem or shell state —
        // the same answer in either fence.
        (Predicate::Task(id), _) => crate::tasks::get(*id)
            .map(|t| t.status.is_terminal())
            // An id that never existed is not going to finish; treating
            // it as "already done" ends the monitor instead of spinning
            // to the deadline.
            .unwrap_or(true),
    }
}

/// Start a monitor. Returns the task id it registered under — the same
/// id space as subagents and shells, so `/tasks` lists it and the one
/// cancel path stops it.
pub fn start(pred: Predicate, deadline_secs: Option<u64>, session_id: &str) -> u32 {
    let label = pred.describe();
    let (id, cancel) = crate::tasks::start(TaskKind::Monitor, &label, session_id);
    let deadline = deadline_secs
        .unwrap_or(DEFAULT_DEADLINE_SECS)
        .clamp(1, MAX_DEADLINE_SECS);

    // For a File predicate, remember whether it already existed: "the
    // build output appeared" and "the build output changed" are
    // different questions and the caller means whichever is true now.
    let baseline = match &pred {
        Predicate::File(p) => std::fs::metadata(p).ok().and_then(|m| m.modified().ok()),
        _ => None,
    };

    // The fence is decided HERE, from the session's principal — never
    // from the monitor's arguments. A below-admin caller's predicate
    // evaluates inside their sandbox, where the host's files and shell
    // are not reachable, which is the same contract `terminal` and
    // `read_file` keep.
    let zone = crate::user_envs::zone_for(Some(session_id), crate::user_envs::ZoneArg::Unset);
    let parent = session_id.to_string();
    crate::bg::spawn(async move {
        let fence = match zone {
            crate::user_envs::Zone::Host => Fence::Host,
            crate::user_envs::Zone::Sandbox { key, name: _ } => {
                // The container-side baseline has to be read with an
                // await, so it happens here rather than in `start`.
                let baseline = match &pred {
                    Predicate::File(p) => env_mtime(&key, p).await,
                    _ => None,
                };
                Fence::Sandbox { key, baseline }
            }
        };
        let started = Instant::now();
        let limit = Duration::from_secs(deadline);
        let outcome = loop {
            if cancel.is_cancelled() {
                break "cancelled";
            }
            if holds(&pred, baseline, &fence).await {
                break "fired";
            }
            if started.elapsed() >= limit {
                break "expired";
            }
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
        };

        crate::tasks::finish(id, outcome == "fired");
        let status = crate::tasks::get(id)
            .map(|t| t.status)
            .unwrap_or(TaskStatus::Done);

        // A cancelled monitor is not news — the caller asked for it to
        // stop, so waking the conversation about it is noise.
        if outcome == "cancelled" || status == TaskStatus::Cancelled {
            return;
        }
        crate::delegate::push_background_event(
            &parent,
            crate::delegate::BackgroundEvent {
                child_id: id,
                kind: TaskKind::Monitor,
                goal: pred.describe(),
                result: json!({
                    "status": outcome,
                    "waited_s": started.elapsed().as_secs(),
                    "summary": match outcome {
                        "fired" => format!("condition met after {}s", started.elapsed().as_secs()),
                        _ => format!("no condition after {deadline}s — gave up"),
                    },
                }),
                child_session: None,
                finished_at: std::time::SystemTime::now(),
            },
        );
        // The same contract as `shell-complete` on the session bus: a
        // monitor finishing is a background-lifecycle fact, published
        // fire-and-forget so the gateway/desktop wake loops can start
        // the session's turn (the busy case is already the drain).
        crate::session_tap::publish(
            &parent,
            "monitor-fired",
            json!({
                "session_id": parent,
                "id": id,
                "condition": pred.describe(),
                "status": outcome,
            }),
        );
    });

    id
}

/// Live monitors, for the `monitor` tool's `list` action.
pub fn list() -> Vec<Value> {
    crate::tasks::live()
        .into_iter()
        .filter(|t| t.kind == TaskKind::Monitor)
        .map(|t| json!({"id": t.id, "waiting_for": t.label, "elapsed_s": t.elapsed_secs()}))
        .collect()
}

pub fn cancel(id: u32) -> bool {
    match crate::tasks::get(id) {
        Some(t) if t.kind == TaskKind::Monitor => crate::tasks::cancel(id),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn settle(id: u32, ticks: u32) {
        for _ in 0..ticks {
            if crate::tasks::get(id).map(|t| t.status) != Some(TaskStatus::Running) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    #[test]
    fn predicate_parsing_demands_exactly_one() {
        assert!(Predicate::from_args(&json!({})).is_err());
        assert!(Predicate::from_args(&json!({"shell": "true", "task": 1})).is_err());
        assert!(matches!(
            Predicate::from_args(&json!({"shell": "true"})),
            Ok(Predicate::Shell(_))
        ));
        assert!(matches!(
            Predicate::from_args(&json!({"task": 7})),
            Ok(Predicate::Task(7))
        ));
        // Whitespace-only is not a command.
        assert!(Predicate::from_args(&json!({"shell": "   "})).is_err());
    }

    #[tokio::test]
    async fn a_true_shell_predicate_fires() {
        // `true` is a unix program; `exit 0` is true in every shell.
        let id = start(Predicate::Shell("exit 0".into()), Some(30), "sess-m1");
        settle(id, 60).await;
        assert_eq!(crate::tasks::get(id).unwrap().status, TaskStatus::Done);
    }

    /// A below-admin caller's monitor evaluates INSIDE their sandbox,
    /// so a host command and a host path are both invisible to it.
    /// Until 2026-09-09 `monitor` had no capability and no zone: a
    /// `shell` predicate was a host shell, and a `file` predicate
    /// stat'd the host filesystem, for any principal who could chat.
    ///
    /// Under `--lib` the env manager is deliberately offline (it must
    /// not create real containers in a developer's podman storage), so
    /// the container calls fail — which is the correct observation
    /// here: the predicate must NOT fire, where on the host it
    /// trivially would. Deleting the `zone_for` call in `start` makes
    /// both halves fire and turns this red.
    #[tokio::test]
    async fn a_sandboxed_monitor_cannot_see_the_host() {
        let sid = "sess-monitor-fence";
        crate::acl::set_session_principal(
            sid,
            crate::acl::Principal::known("irc:tester", crate::acl::Role::User),
        );
        // A host command that always succeeds…
        let id = start(Predicate::Shell("true".into()), Some(1), sid);
        settle(id, 60).await;
        assert_eq!(
            crate::tasks::get(id).unwrap().status,
            TaskStatus::Error,
            "a sandboxed `shell` predicate must not run on the host"
        );
        // …and a host path that certainly exists.
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("already-here");
        std::fs::write(&path, b"x").expect("write");
        let id = start(
            Predicate::File(path.to_string_lossy().to_string()),
            Some(1),
            sid,
        );
        settle(id, 60).await;
        assert_eq!(
            crate::tasks::get(id).unwrap().status,
            TaskStatus::Error,
            "a sandboxed `file` predicate must not stat the host"
        );
    }

    #[tokio::test]
    async fn a_never_true_predicate_expires_rather_than_spinning() {
        let id = start(Predicate::Shell("false".into()), Some(1), "sess-m2");
        settle(id, 60).await;
        assert_eq!(
            crate::tasks::get(id).unwrap().status,
            TaskStatus::Error,
            "an expired monitor is not a success"
        );
    }

    #[tokio::test]
    async fn cancel_stops_a_monitor_and_stays_quiet() {
        let id = start(Predicate::Shell("false".into()), Some(600), "sess-m3");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(cancel(id));
        settle(id, 60).await;
        assert_eq!(crate::tasks::get(id).unwrap().status, TaskStatus::Cancelled);
        // A cancelled monitor must not wake the conversation.
        let evs = crate::delegate::take_background_events("sess-m3");
        assert!(evs.is_empty(), "cancelling should not queue an event");
    }

    #[tokio::test]
    async fn a_task_predicate_follows_another_task() {
        let (watched, _tok) = crate::tasks::start(TaskKind::Shell, "pretend", "sess-m4");
        let id = start(Predicate::Task(watched), Some(30), "sess-m4");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            crate::tasks::get(id).unwrap().status,
            TaskStatus::Running,
            "must still be waiting while the watched task runs"
        );
        crate::tasks::finish(watched, true);
        settle(id, 60).await;
        assert_eq!(crate::tasks::get(id).unwrap().status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn a_file_predicate_fires_when_the_path_appears() {
        let dir = std::env::temp_dir().join(format!("veles-mon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("appears.txt");
        let _ = std::fs::remove_file(&path);

        let id = start(
            Predicate::File(path.display().to_string()),
            Some(30),
            "sess-m5",
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(&path, b"here").unwrap();
        settle(id, 60).await;
        assert_eq!(crate::tasks::get(id).unwrap().status, TaskStatus::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_refuses_a_non_monitor_id() {
        let (shell_id, _) = crate::tasks::start(TaskKind::Shell, "x", "sess-m6");
        assert!(!cancel(shell_id), "monitor cancel must not stop a shell");
        assert!(!cancel(999_999));
    }
}
