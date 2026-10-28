//! Task registry — one place for every long-running thing the agent
//! started and has not finished.
//!
//! Three kinds share this registry because they share every surface:
//!
//!   - [`TaskKind::Subagent`] — a `delegate_task` child (`delegate.rs`)
//!   - [`TaskKind::Shell`]    — a background command (`shells.rs`)
//!   - [`TaskKind::Monitor`]  — a wait-for-condition (`monitor.rs`)
//!
//! WHY ONE REGISTRY. This replaces `review.rs`'s `SubagentRegistry`,
//! which was written on every delegate call and READ BY NOTHING — the
//! 2026-09-02 audit (PENDING.md) found the chain terminated at
//! `background_children_for`, which had no callers and ignored its own
//! `session_id` argument because the registry had no session key. Three
//! separate mechanisms each growing their own half-wired registry is
//! how that happens twice. One registry means one formatter, one cancel
//! path, and one reader per frontend instead of three.
//!
//! THE STANDING RULE. USER: "we need the visibility of all of the
//! backend stuff in all frontends". [`listing`] is the single text
//! formatter for CLI/REPL/TUI/gateway (the `Agent::format_cache_line`
//! precedent) and [`snapshot`] is its JSON twin for the desktop API.
//! Adding a surface must never mean adding a second formatter.

use crate::lockutil::LockOk;
use crate::providers::CancelToken;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::SystemTime;

/// Finished records kept for inspection before the oldest are dropped.
/// The old `SubagentRegistry` had no cap and grew for the life of the
/// process; a long-lived gateway would accumulate every child forever.
const KEEP_FINISHED: usize = 100;

/// Labels are shown in list output on one line — cap them at the source
/// so no frontend has to.
const LABEL_CAP: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TaskKind {
    Subagent,
    Shell,
    Monitor,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::Subagent => "subagent",
            TaskKind::Shell => "shell",
            TaskKind::Monitor => "monitor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Done,
    Error,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Error => "error",
            TaskStatus::Cancelled => "cancelled",
        }
    }
    /// Terminal states — a monitor waiting on a task watches for these.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: u32,
    pub kind: TaskKind,
    /// Goal, command or predicate — whatever identifies this to a human.
    pub label: String,
    /// Parent session id. The old registry had no such field, which is
    /// why `background_children_for(session_id)` had to ignore its
    /// argument and answer process-wide.
    pub session: String,
    /// For a Subagent: the child SESSION id it runs in. A subagent is a
    /// real session the user can attach to and review (USER taxonomy,
    /// 2026-09-17); the registry row is how a listing offers the link.
    /// None for shells and monitors, which have no session.
    pub child_session: Option<String>,
    pub started: SystemTime,
    pub finished: Option<SystemTime>,
    pub status: TaskStatus,
    /// Cooperative cancellation. Whoever owns the work observes this;
    /// for a subagent that is the `try_wait` poll loop in delegate.rs,
    /// which translates it into `child.kill()`.
    pub cancel: CancelToken,
}

impl TaskRecord {
    /// Seconds since start, or the full run time once finished.
    pub fn elapsed_secs(&self) -> u64 {
        let end = self.finished.unwrap_or_else(SystemTime::now);
        end.duration_since(self.started)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

pub struct TaskRegistry {
    next_id: u32,
    tasks: Vec<TaskRecord>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            tasks: Vec::new(),
        }
    }

    /// Register a new running task and return its id. The returned
    /// [`CancelToken`] is the same one stored in the record, so the
    /// caller can hand it straight to the work it just started.
    pub fn start(&mut self, kind: TaskKind, label: &str, session: &str) -> (u32, CancelToken) {
        let id = self.next_id;
        self.next_id += 1;
        let cancel = CancelToken::new();
        self.tasks.push(TaskRecord {
            id,
            kind,
            label: label.chars().take(LABEL_CAP).collect(),
            session: session.to_string(),
            child_session: None,
            started: SystemTime::now(),
            finished: None,
            status: TaskStatus::Running,
            cancel: cancel.clone(),
        });
        self.prune();
        (id, cancel)
    }

    /// Mark a task finished. A task whose token was flipped reports as
    /// `Cancelled` regardless of how the underlying work exited — a
    /// killed child exits non-zero, and calling that an error would
    /// misreport every successful cancel.
    pub fn finish(&mut self, id: u32, ok: bool) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.status = if t.cancel.is_cancelled() {
                TaskStatus::Cancelled
            } else if ok {
                TaskStatus::Done
            } else {
                TaskStatus::Error
            };
            t.finished = Some(SystemTime::now());
        }
        // Also prune here, not only in `start`: a burst that finishes
        // after the last start would otherwise sit one over the cap.
        self.prune();
    }

    /// Flip a task's cancel token. Returns false for an unknown id or
    /// one that has already finished — the caller reports "not found"
    /// vs "already done" from that.
    pub fn cancel(&mut self, id: u32) -> bool {
        match self.tasks.iter().find(|t| t.id == id) {
            Some(t) if t.status == TaskStatus::Running => {
                t.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    pub fn get(&self, id: u32) -> Option<TaskRecord> {
        self.tasks.iter().find(|t| t.id == id).cloned()
    }

    /// Stamp the child SESSION id onto a subagent's row. Called right
    /// after `start` by the spawn paths, which mint the child session
    /// first (so it is attachable from the moment the task exists).
    /// Unknown ids are a no-op: the registry prunes finished rows, and
    /// a stamp racing a prune must not resurrect anything.
    pub fn set_child_session(&mut self, id: u32, session: &str) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.child_session = Some(session.to_string());
        }
    }

    pub fn live(&self) -> Vec<TaskRecord> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .cloned()
            .collect()
    }

    pub fn all(&self) -> Vec<TaskRecord> {
        self.tasks.clone()
    }

    /// Live tasks started by one session. Honest, unlike the function
    /// this replaces.
    pub fn for_session(&self, session: &str) -> Vec<TaskRecord> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running && t.session == session)
            .cloned()
            .collect()
    }

    /// Drop the oldest finished records past [`KEEP_FINISHED`]. Running
    /// tasks are never pruned.
    fn prune(&mut self) {
        let finished = self.tasks.iter().filter(|t| t.status.is_terminal()).count();
        if finished <= KEEP_FINISHED {
            return;
        }
        let mut to_drop = finished - KEEP_FINISHED;
        self.tasks.retain(|t| {
            if to_drop > 0 && t.status.is_terminal() {
                to_drop -= 1;
                false
            } else {
                true
            }
        });
    }

    pub fn snapshot(&self) -> Value {
        let tasks: Vec<Value> = self
            .tasks
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "kind": t.kind.as_str(),
                    "label": t.label,
                    "session": t.session,
                    "status": t.status.as_str(),
                    "elapsed_s": t.elapsed_secs(),
                    // The child session id, when the kind has one (the
                    // desktop's "attach" link rides this).
                    "child_session": t.child_session,
                })
            })
            .collect();
        json!({
            "tasks": tasks,
            "live": self.live().len(),
            "total_tracked": self.tasks.len(),
        })
    }
}

/// Process-wide registry. Every kind of task registers here so one
/// `/tasks` surface answers for all of them.
pub fn registry() -> &'static Mutex<TaskRegistry> {
    static REG: std::sync::OnceLock<Mutex<TaskRegistry>> = std::sync::OnceLock::new();
    REG.get_or_init(|| Mutex::new(TaskRegistry::new()))
}

// ── convenience wrappers (the API every caller actually uses) ─────────

pub fn start(kind: TaskKind, label: &str, session: &str) -> (u32, CancelToken) {
    registry().lock_ok().start(kind, label, session)
}

pub fn set_child_session(id: u32, session: &str) {
    registry().lock_ok().set_child_session(id, session);
}

pub fn finish(id: u32, ok: bool) {
    registry().lock_ok().finish(id, ok);
}

pub fn cancel(id: u32) -> bool {
    registry().lock_ok().cancel(id)
}

pub fn get(id: u32) -> Option<TaskRecord> {
    registry().lock_ok().get(id)
}

pub fn live() -> Vec<TaskRecord> {
    registry().lock_ok().live()
}

pub fn all() -> Vec<TaskRecord> {
    registry().lock_ok().all()
}

/// Cancel `id` only if the calling session may reach it. Task ids are
/// small integers in a PROCESS-wide space and `/tasks cancel <id>` sits
/// at the GUEST capability floor, so the unscoped `cancel` let anyone
/// in a shared channel stop anyone else's work by guessing a number.
/// Same rule as `shells::may_reach`: your own session, or you are the
/// owner (and an owner SURFACE, which has no principal, keeps its
/// process-wide reach).
pub fn cancel_for(id: u32, session_id: &str) -> bool {
    match get(id) {
        Some(rec) if crate::shells::may_reach(session_id, &rec.session) => cancel(id),
        _ => false,
    }
}

pub fn for_session(session: &str) -> Vec<TaskRecord> {
    registry().lock_ok().for_session(session)
}

pub fn snapshot() -> Value {
    registry().lock_ok().snapshot()
}

/// THE text formatter. CLI, REPL, TUI and gateway all render tasks
/// through this and nothing else. The aggregate view: every kind in one
/// table. `veles tasks`, `GET /api/tasks` and the Tcl bridge op all
/// mean this.
pub fn listing() -> String {
    listing_of(None)
}

/// One kind's slice of the same table — what `/subagents` and
/// `/monitors` render. The chat taxonomy (USER, 2026-09-17) split the
/// running-work registry by kind, and `/tasks` became the TODO list;
/// the aggregate listing stays for the CLI and the API.
pub fn listing_kind(kind: TaskKind) -> String {
    listing_of(Some(kind))
}

fn listing_of(kind: Option<TaskKind>) -> String {
    let live: Vec<TaskRecord> = live()
        .into_iter()
        .filter(|t| kind.is_none_or(|k| t.kind == k))
        .collect();
    let (header, none) = match kind {
        None => ("tasks_header", "tasks_none"),
        Some(TaskKind::Subagent) => ("subagents_header", "subagents_none"),
        Some(TaskKind::Shell) => ("shells_header", "shells_none"),
        Some(TaskKind::Monitor) => ("monitors_header", "monitors_none"),
    };
    if live.is_empty() {
        return crate::i18n::lookup(none, &[]);
    }
    let mut out = crate::i18n::lookup(header, &[]);
    out.push('\n');
    for t in &live {
        out.push_str(&format!(
            "  [{}] {:<8} {}s  {}",
            t.id,
            t.kind.as_str(),
            t.elapsed_secs(),
            t.label
        ));
        // A subagent IS a session (USER taxonomy, 2026-09-17) — name it
        // so a listing is also the way to attach: `/resume <child>` from
        // chat, the desktop's attach link reads the same id from
        // /api/tasks. Shells and monitors have no session to name.
        if let Some(cs) = &t.child_session {
            out.push_str(&format!("  (session {cs})"));
        }
        out.push('\n');
    }
    out
}

/// The per-kind command body: `/subagents` and `/monitors` are the same
/// parser twice, differing only in which slice they list and which
/// family they point a mismatched cancel at. `cancel <id>` is scoped to
/// the calling session exactly like the aggregate command was (ids are
/// process-wide and this sits at the capability floor); a cancel aimed
/// at the WRONG KIND is refused with the family that owns it, because
/// "task 3 is a shell — /shells kill 3" beats cancelling work the
/// asker cannot see the kind of.
///
/// This replaces the old `/tasks` body. The aggregate listing survives
/// as [`listing`] for the CLI and the API; the aggregate COMMAND had
/// exactly one caller, and that caller is now the TODO list.
pub fn kind_command(rest: &str, session_id: &str, kind: TaskKind) -> String {
    // The family a KIND is listed/cancelled under. Used twice: for the
    // caller's own verbs, and — in the mismatch message — to name the
    // family that actually OWNS a task whose kind differs, so the asker
    // is pointed at `/shells kill 3` rather than told their own family
    // owns something it cannot see.
    fn family_of(k: TaskKind) -> &'static str {
        match k {
            TaskKind::Subagent => "/subagents",
            TaskKind::Shell => "/shells",
            TaskKind::Monitor => "/monitors",
        }
    }
    let family = family_of(kind);
    let mut parts = rest.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("cancel"), Some(id)) => match id.parse::<u32>() {
            Ok(i) => match get(i) {
                Some(rec) if rec.kind != kind => crate::i18n::lookup(
                    "tasks_kind_mismatch",
                    &[
                        ("id", i.to_string()),
                        ("kind", rec.kind.as_str().to_string()),
                        ("cmd", family_of(rec.kind).to_string()),
                    ],
                ),
                _ if cancel_for(i, session_id) => {
                    crate::i18n::lookup("tasks_cancelled", &[("id", i.to_string())])
                }
                _ => crate::i18n::lookup("tasks_not_running", &[("id", i.to_string())]),
            },
            Err(_) => format!("usage: {family} cancel <id>"),
        },
        (Some(sub), _) => {
            format!("unknown {family} subcommand '{sub}' — try cancel <id>")
        }
        (None, _) => listing_kind(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-wide, so tests that assert on global
    /// state would race. Each test drives its own instance instead;
    /// the free functions are one-line wrappers over these methods.
    #[test]
    fn start_finish_and_live() {
        let mut r = TaskRegistry::new();
        let (a, _) = r.start(TaskKind::Subagent, "goal a", "sess-1");
        let (b, _) = r.start(TaskKind::Shell, "sleep 300", "sess-1");
        assert_eq!(r.live().len(), 2);
        r.finish(a, true);
        assert_eq!(r.get(a).unwrap().status, TaskStatus::Done);
        assert_eq!(r.live().len(), 1);
        assert_eq!(r.live()[0].id, b);
    }

    #[test]
    fn cancel_flips_the_token_and_the_status() {
        let mut r = TaskRegistry::new();
        let (id, tok) = r.start(TaskKind::Shell, "sleep 300", "sess-1");
        assert!(!tok.is_cancelled());
        assert!(r.cancel(id));
        assert!(
            tok.is_cancelled(),
            "the caller's token must be the record's"
        );

        // A killed child exits non-zero. Reporting that as Error would
        // make every successful cancel look like a failure.
        r.finish(id, false);
        assert_eq!(r.get(id).unwrap().status, TaskStatus::Cancelled);
    }

    #[test]
    fn cancel_is_false_for_unknown_and_finished() {
        let mut r = TaskRegistry::new();
        assert!(!r.cancel(999), "unknown id");
        let (id, _) = r.start(TaskKind::Subagent, "g", "s");
        r.finish(id, true);
        assert!(!r.cancel(id), "already finished");
    }

    #[test]
    fn for_session_scopes_and_excludes_finished() {
        let mut r = TaskRegistry::new();
        let (a, _) = r.start(TaskKind::Subagent, "a", "sess-1");
        r.start(TaskKind::Subagent, "b", "sess-2");
        r.start(TaskKind::Shell, "c", "sess-1");
        assert_eq!(r.for_session("sess-1").len(), 2);
        assert_eq!(r.for_session("sess-2").len(), 1);
        assert!(r.for_session("nobody").is_empty());
        r.finish(a, true);
        assert_eq!(r.for_session("sess-1").len(), 1, "finished drops out");
    }

    #[test]
    fn prune_caps_finished_but_never_running() {
        let mut r = TaskRegistry::new();
        let (keep, _) = r.start(TaskKind::Shell, "long runner", "s");
        for i in 0..(KEEP_FINISHED + 50) {
            let (id, _) = r.start(TaskKind::Subagent, &format!("g{i}"), "s");
            r.finish(id, true);
        }
        let all = r.all();
        assert_eq!(
            all.iter().filter(|t| t.status.is_terminal()).count(),
            KEEP_FINISHED
        );
        assert!(
            all.iter().any(|t| t.id == keep),
            "a running task must survive pruning"
        );
    }

    #[test]
    fn labels_are_capped() {
        let mut r = TaskRegistry::new();
        let (id, _) = r.start(TaskKind::Subagent, &"x".repeat(500), "s");
        assert_eq!(r.get(id).unwrap().label.chars().count(), LABEL_CAP);
    }

    #[test]
    fn snapshot_reports_kinds_and_counts() {
        let mut r = TaskRegistry::new();
        let (a, _) = r.start(TaskKind::Subagent, "a", "s");
        r.start(TaskKind::Monitor, "wait for ci", "s");
        r.finish(a, true);
        let snap = r.snapshot();
        assert_eq!(snap["live"], 1);
        assert_eq!(snap["total_tracked"], 2);
        assert_eq!(snap["tasks"][1]["kind"], "monitor");
    }

    /// The per-kind command body (`/subagents`, `/monitors`) — the
    /// shared `/tasks` body's replacement now that `/tasks` is the TODO
    /// list. These paths stay off the global registry (bad ids, unknown
    /// subcommands), so they cannot race the instance-based tests.
    #[test]
    fn kind_command_parses_subcommands() {
        let sid = "sess-kind-cmd";
        let s = TaskKind::Subagent;
        assert!(
            kind_command("cancel abc", sid, s).contains("usage:"),
            "bad id"
        );
        assert!(
            kind_command("cancel 999999", sid, s).contains("999999"),
            "unknown id names the id"
        );
        assert!(
            kind_command("frobnicate", sid, s).contains("unknown /subagents subcommand"),
            "unknown verb names the family, not a generic '/tasks'"
        );
        assert!(
            !kind_command("", sid, s).is_empty(),
            "no argument lists (possibly empty table)"
        );
    }

    /// `/tasks output <id>` existed on the old aggregate command, but a
    /// subagent and a monitor have no captured output to show — only a
    /// shell does. The per-kind commands must NOT carry that verb over,
    /// or `/subagents output 3` would imply a subagent has a transcript
    /// you can read this way (it does not; that is the child SESSION).
    #[test]
    fn kind_command_has_no_output_verb() {
        let out = kind_command("output 1", "sess-k", TaskKind::Monitor);
        assert!(
            out.contains("unknown /monitors subcommand 'output'"),
            "`output` must be refused as unknown on a non-shell family, got: {out}"
        );
    }

    /// A cancel aimed at the wrong KIND names the family that owns it.
    /// Ids are process-wide across the three kinds, so `/monitors
    /// cancel 3` where 3 is a shell must not silently kill the shell —
    /// it points at `/shells kill 3` instead. Drives the GLOBAL registry
    /// (kind_command reads `get`), so it takes the testenv lock.
    #[test]
    fn kind_command_refuses_a_mismatched_kind() {
        let _g = crate::testenv::lock();
        let (id, _tok) = start(TaskKind::Shell, "sleep 300", "sess-kind-mismatch");
        let out = kind_command(
            &format!("cancel {id}"),
            "sess-kind-mismatch",
            TaskKind::Monitor,
        );
        assert!(
            out.contains("shell") && out.contains("/shells"),
            "a monitor cancel of a shell id must name the shell family, got: {out}"
        );
        assert!(
            crate::tasks::get(id).map(|t| t.status) == Some(TaskStatus::Running),
            "the mismatched cancel must NOT have stopped the shell"
        );
        cancel_for(id, "sess-kind-mismatch"); // tidy up the live row
    }

    /// `/tasks cancel <id>` sits at the GUEST capability floor and the
    /// id space is process-wide, so a guest in a shared channel could
    /// stop anyone's work by guessing a number. Cancelling is scoped to
    /// the calling session (owner surfaces, which carry no principal,
    /// keep their process-wide reach).
    #[test]
    fn cancel_is_scoped_to_the_calling_session() {
        let _g = crate::testenv::lock();
        let (id, _tok) = start(TaskKind::Subagent, "someone else's work", "sess-A");
        crate::acl::set_session_principal(
            "sess-B",
            crate::acl::Principal::known("irc:eve", crate::acl::Role::User),
        );
        assert!(
            !cancel_for(id, "sess-B"),
            "a stranger's session must not cancel this task"
        );
        assert_eq!(get(id).map(|t| t.status), Some(TaskStatus::Running));
        assert!(cancel_for(id, "sess-A"), "its own session may cancel it");
    }
}
