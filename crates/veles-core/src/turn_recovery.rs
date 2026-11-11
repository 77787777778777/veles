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

use crate::shell::Dialect;
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

/// Why a tool call cut by the restart may be answered and the turn
/// continued without the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutCall {
    /// The call only reads: nothing changed, whatever point it reached.
    ReadOnly,
    /// The call was this gateway's own restart (and reads around it):
    /// the restart it asked for is what cut it.
    Restart,
}

/// The shell a cut `terminal` call ran in, as far as its command line
/// depends on it: how it splits the line, the PATH it searched, and the
/// executable this gateway is.
struct CutShell<'a> {
    dialect: Dialect,
    path: Option<&'a std::ffi::OsStr>,
    exe: Option<&'a std::path::Path>,
}

/// Is this simple command the gateway's own restart — `<this gateway's
/// executable> gateway restart`, or `systemctl --user restart` of its
/// unit (`veles-gateway`, `veles-gateway-dev`)? Any other binary with
/// that argv (a `veles` the agent built or downloaded) did something
/// nobody knows, and goes to review.
fn restarts_the_gateway(words: &[&str], shell: &CutShell) -> bool {
    match words {
        [bin, "gateway", "restart"] => shell
            .exe
            .is_some_and(|exe| names_this_executable(bin, shell.dialect, shell.path, exe)),
        ["systemctl", "--user", "restart", unit] => unit
            .trim_end_matches(".service")
            .starts_with("veles-gateway"),
        _ => false,
    }
}

/// Does `bin`, as a shell of `dialect` would run it with `path` as its
/// PATH, name the file `exe`? An absolute path does when it is that file
/// (`C:/…` and `C:\…` alike, in any letter case Windows accepts); a bare
/// name when PATH resolves it to that file (`veles` on PATH is this
/// gateway, or it is not; on Windows `veles` and `veles.exe` alike,
/// through PATHEXT). A relative path depends on the working directory
/// the cut shell had, which is not known here, so it does not; for the
/// same reason neither does a bare name to cmd, which looks in that
/// directory before PATH (PowerShell and POSIX shells do not).
fn names_this_executable(
    bin: &str,
    dialect: Dialect,
    path: Option<&std::ffi::OsStr>,
    exe: &std::path::Path,
) -> bool {
    let p = std::path::Path::new(bin);
    let found = if p.is_absolute() {
        Some(p.to_path_buf())
    } else if p.components().count() == 1 && dialect != Dialect::Cmd {
        let candidates = crate::platform::exe_candidates(bin);
        path.and_then(|path| {
            std::env::split_paths(path)
                .flat_map(|dir| candidates.iter().map(move |c| dir.join(c)))
                .find(|c| c.is_file())
        })
    } else {
        None
    };
    let same = |a: &std::path::Path| std::fs::canonicalize(a).ok();
    found.is_some_and(|f| same(&f).is_some() && same(&f) == same(exe))
}

/// May a turn cut inside `tool` (called with `args`, its result never
/// saved) continue without the owner? Only on the engine's existing
/// judgements, never a list of its own:
///
/// - the approval screen's: [`crate::approval::classify_all`] finds no
///   hazard, so no approval was being asked inside the call;
/// - plan mode's: [`crate::plan::refuses`] lets the call run, and for a
///   shell command every part is on plan mode's read-only allowlist;
/// - the ACL's: outside the shell, the tool's capability
///   ([`crate::acl::tool_cap`]) is one a guest holds (`Role::Guest`,
///   "read-only builtins"), but not `chat`, whose tools (clarify,
///   present_plan, batch) ask the human or run other calls. A module
///   tool is judged by the tier its module declared
///   ([`crate::acl::declare_tool_cap`], read before the builtin table),
///   so one that declares file reads counts as read-only here, as it
///   does for a guest. An MCP tool (`mcp`, which no guest holds) and a
///   tool nothing classified go to review.
///
/// The gateway's own restart is the one addition: a shell command whose
/// every other part is read-only by plan mode's judgement, run with this
/// gateway's own executable (its path, or a bare name PATH resolves to
/// it) or as `systemctl --user restart` of its unit. Approvals do
/// screen a `systemctl … restart` (service control, and it names
/// veles), and that judgement is not needed here: whether that approval
/// was answered or still open, the gateway did restart, which is all
/// the command could do.
///
/// Anything else has an unknown effect and goes to review.
pub fn cut_call(tool: &str, args: &serde_json::Value) -> Option<CutCall> {
    let screened = crate::approval::classify_all(tool, args).is_empty();
    if tool == "terminal" {
        let cmd = args.get("command").and_then(|c| c.as_str())?;
        let exe = std::env::current_exe().ok();
        let path = std::env::var_os("PATH");
        let shell = CutShell {
            dialect: crate::shell::dialect(),
            path: path.as_deref(),
            exe: exe.as_deref(),
        };
        return cut_command(cmd, screened, &shell);
    }
    if !screened || crate::plan::refuses(tool, args, None).is_some() {
        return None;
    }
    let cap = crate::acl::tool_cap(tool)?;
    (cap != crate::acl::Cap::Chat && crate::acl::Role::Guest.default_caps().contains(&cap))
        .then_some(CutCall::ReadOnly)
}

/// [`cut_call`]'s answer for a `terminal` command that ran in `shell`,
/// its line split as that shell splits it.
fn cut_command(cmd: &str, screened: bool, shell: &CutShell) -> Option<CutCall> {
    let judge = |also: &dyn Fn(&[&str]) -> bool| {
        crate::plan::judge_command_allowing(cmd, shell.dialect, also).is_ok()
    };
    if judge(&|_| false) {
        return screened.then_some(CutCall::ReadOnly);
    }
    judge(&|words| restarts_the_gateway(words, shell)).then_some(CutCall::Restart)
}

/// Where the call recorded as running when the process died stands.
#[derive(Debug, Clone, PartialEq)]
pub enum InFlight {
    /// Its result was saved: nothing was cut.
    Answered,
    /// It was cut: its tool name and arguments, for [`cut_call`] to judge.
    Cut(String, serde_json::Value),
    /// No call of the turn carries the id, or the call cannot be read.
    Unknown,
}

/// The call `id` that was running when a restart cut the turn, judged by
/// POSITION, never by id alone. Ids are not unique (ENG-1): a route that
/// sends none leaves `""`, and models that number calls per response
/// open every round with `terminal:0`, so an EARLIER call's saved result
/// can carry the cut call's id. The round in flight is the last run of
/// adjacent assistant rows declaring the id; the agent runs its calls in
/// declaration order, so with `k` results carrying the id after it, the
/// cut call is the (k+1)-th call carrying the id, and with as many
/// results as calls nothing was cut.
pub fn call_in_flight(rows: &[&crate::providers::ChatMessage], id: &str) -> InFlight {
    use serde_json::Value;
    let calls = |m: &crate::providers::ChatMessage| -> Vec<Value> {
        if m.role != "assistant" {
            return Vec::new();
        }
        m.tool_calls
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let carries = |call: &Value| call.get("id").and_then(Value::as_str).unwrap_or("") == id;
    let Some(end) = rows.iter().rposition(|m| calls(m).iter().any(carries)) else {
        return InFlight::Unknown;
    };
    let mut start = end;
    while start > 0 && !calls(rows[start - 1]).is_empty() {
        start -= 1;
    }
    let answered = rows[end + 1..]
        .iter()
        .filter(|m| {
            m.role == "tool"
                && m.tool_meta
                    .as_ref()
                    .is_some_and(|meta| meta.tool_call_id == id)
        })
        .count();
    let carrying: Vec<Value> = rows[start..=end]
        .iter()
        .flat_map(|m| calls(m))
        .filter(carries)
        .collect();
    let Some(call) = carrying.get(answered) else {
        return InFlight::Answered;
    };
    let f = call.get("function").unwrap_or(call);
    let Some(name) = f.get("name").and_then(Value::as_str) else {
        return InFlight::Unknown;
    };
    let args = match f.get("arguments").or_else(|| f.get("input")) {
        Some(Value::String(s)) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => return InFlight::Unknown,
        },
        Some(v) => v.clone(),
        None => Value::Null,
    };
    InFlight::Cut(name.to_string(), args)
}

/// The result a cut call is answered with, so the model knows what
/// happened to it.
pub fn cut_call_result(kind: CutCall) -> &'static str {
    match kind {
        CutCall::ReadOnly => {
            "[veles] the gateway restarted before this call returned, and its result was lost. \
             The call only reads, so nothing changed; run it again if you still need it."
        }
        CutCall::Restart => {
            "[veles] the gateway restarted: this command restarts it, so the restart it asked \
             for happened. Its output was lost with the old process; do not restart again \
             unless something still needs it."
        }
    }
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

    /// The running call is found by position: an earlier saved result
    /// with the same id is not its result (ENG-1 ids: `""`, reused
    /// per-response numbers).
    #[test]
    fn the_call_in_flight_is_found_by_position_not_by_id() {
        let round = |calls: &[(&str, &str)]| {
            let mut m = ChatMessage::assistant("", None);
            m.tool_calls = Some(serde_json::Value::Array(
                calls
                    .iter()
                    .map(|(id, cmd)| {
                        serde_json::json!({"id": id, "type": "function", "function": {
                            "name": "terminal",
                            "arguments": serde_json::json!({"command": cmd}).to_string()}})
                    })
                    .collect(),
            ));
            m
        };
        let result = |id: &str| ChatMessage::tool_result(id, "terminal", "out".into());
        let cut = |cmd: &str| InFlight::Cut("terminal".into(), serde_json::json!({"command": cmd}));

        let rows = [
            round(&[("t:0", "ls")]),
            result("t:0"),
            round(&[("t:0", "rm x")]),
        ];
        let rows: Vec<&ChatMessage> = rows.iter().collect();
        assert_eq!(call_in_flight(&rows, "t:0"), cut("rm x"));

        let rows = [round(&[("", "ls"), ("", "rm x")]), result("")];
        let rows: Vec<&ChatMessage> = rows.iter().collect();
        assert_eq!(call_in_flight(&rows, ""), cut("rm x"));

        let rows = [round(&[("", "ls")]), round(&[("", "rm x")]), result("")];
        let rows: Vec<&ChatMessage> = rows.iter().collect();
        assert_eq!(
            call_in_flight(&rows, ""),
            cut("rm x"),
            "adjacent rows are one round"
        );

        let rows = [round(&[("a", "ls")]), result("a")];
        let rows: Vec<&ChatMessage> = rows.iter().collect();
        assert_eq!(call_in_flight(&rows, "a"), InFlight::Answered);
        assert_eq!(call_in_flight(&rows, "b"), InFlight::Unknown);
    }

    /// Only THIS gateway's own restart continues without review. Any
    /// binary whose base name was `veles` counted: a `veles` the model
    /// built or downloaded, run as `/tmp/x/veles gateway restart`, was
    /// taken for the gateway's own restart and the turn went on as if
    /// nothing else could have happened. The running executable, by
    /// its path, is the gateway's restart; another file is review.
    #[test]
    fn only_this_gateways_own_executable_restarts_it() {
        let restart = |cmd: &str| cut_call("terminal", &serde_json::json!({ "command": cmd }));
        let exe = std::env::current_exe().unwrap();
        let own = format!("{} gateway restart", exe.display());
        assert_eq!(restart(&own), Some(CutCall::Restart), "{own}");
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("veles");
        std::fs::write(&other, "#!/bin/sh\n").unwrap();
        let other = format!("{} gateway restart", other.display());
        assert_eq!(restart(&other), None, "another veles is not this gateway");
        assert_eq!(
            restart("/nonexistent/veles gateway restart"),
            None,
            "a path that is nothing at all"
        );
        assert_eq!(
            restart("./veles gateway restart"),
            None,
            "relative to a directory the cut shell had, which is not known"
        );
        assert_eq!(
            restart("cd ~/src && systemctl --user restart veles-gateway"),
            Some(CutCall::Restart),
            "the unit's restart is unchanged"
        );
    }

    /// A bare `veles` is whatever PATH resolves it to: this gateway when
    /// that is its executable (a symlink to it included), and review when
    /// PATH finds another `veles` first or none at all.
    #[cfg(unix)]
    #[test]
    fn a_bare_name_is_this_gateway_only_as_path_resolves_it() {
        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join("veles-real");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let on_path = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&exe, on_path.path().join("veles")).unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("veles"), "#!/bin/sh\n").unwrap();
        let path = |dirs: &[&std::path::Path]| std::env::join_paths(dirs).unwrap();
        let names = |bin: &str, path: Option<&std::ffi::OsStr>| {
            names_this_executable(bin, Dialect::Posix, path, &exe)
        };
        assert!(names("veles", Some(&path(&[on_path.path()]))));
        assert!(
            !names("veles", Some(&path(&[other.path(), on_path.path()]))),
            "PATH finds another veles first"
        );
        assert!(!names("veles", Some(&path(&[bin.path()]))));
        assert!(!names("veles", None));
        assert!(names(exe.to_str().unwrap(), None));
        let via_link = on_path.path().join("veles");
        assert!(names(via_link.to_str().unwrap(), None));
    }

    /// The forms a Windows agent writes for its own restart. The line was
    /// split by POSIX rules, which take `\` for an escape: the running
    /// `C:\…\veles.exe gateway restart` was read as `C:…veles.exe` and
    /// went to review, so the Windows node sent every one of its own
    /// restarts to the owner. Split as PowerShell (the shell there) and
    /// cmd split it, the gateway's executable is itself in every spelling
    /// that names it, and nothing else is.
    #[cfg(windows)]
    #[test]
    fn a_windows_agents_own_restart_is_recognised_as_it_writes_it() {
        use crate::shell::Dialect::{Cmd, Posix, PowerShell};
        let root = tempfile::tempdir().unwrap();
        let exe_in = |dir: &std::path::Path| {
            std::fs::create_dir_all(dir).unwrap();
            let exe = dir.join("veles.exe");
            std::fs::write(&exe, "").unwrap();
            exe
        };
        // A space in the install path, as under "Program Files".
        let install = root.path().join("Program Dir").join("veles");
        let exe = exe_in(&install);
        let plain_exe = exe_in(&root.path().join("bin"));
        let other = root.path().join("other");
        let other_exe = exe_in(&other);

        let on_path = std::env::join_paths([&install]).unwrap();
        let judge = |dialect, exe: &std::path::Path, path: &std::ffi::OsStr, cmd: &str| {
            let shell = CutShell {
                dialect,
                path: Some(path),
                exe: Some(exe),
            };
            cut_command(cmd, true, &shell)
        };
        let quoted = exe.display().to_string();
        let slashes = quoted.replace('\\', "/");
        let bare = plain_exe.display().to_string();
        for (dialect, this, cmd) in [
            (PowerShell, &exe, "veles gateway restart".to_string()),
            (PowerShell, &exe, "veles.exe gateway restart".into()),
            (PowerShell, &exe, "VELES.EXE gateway restart".into()),
            (PowerShell, &exe, format!("& \"{quoted}\" gateway restart")),
            (PowerShell, &exe, format!("& '{quoted}' gateway restart")),
            (PowerShell, &exe, format!("& \"{slashes}\" gateway restart")),
            (
                PowerShell,
                &exe,
                format!("cd ~; & \"{quoted}\" gateway restart"),
            ),
            (Cmd, &exe, format!("\"{quoted}\" gateway restart")),
            (Cmd, &exe, format!("\"{slashes}\" gateway restart")),
            (PowerShell, &plain_exe, format!("{bare} gateway restart")),
            (
                PowerShell,
                &plain_exe,
                format!("{} gateway restart", bare.replace('\\', "/")),
            ),
            (Cmd, &plain_exe, format!("{bare} gateway restart")),
        ] {
            assert_eq!(
                judge(dialect, this, &on_path, &cmd),
                Some(CutCall::Restart),
                "{dialect:?}: {cmd}"
            );
        }

        let other_first = std::env::join_paths([&other, &install]).unwrap();
        for (dialect, this, path, cmd, why) in [
            (
                PowerShell,
                &exe,
                &on_path,
                format!("& \"{}\" gateway restart", other_exe.display()),
                "another veles.exe",
            ),
            (
                PowerShell,
                &exe,
                &other_first,
                "veles gateway restart".to_string(),
                "PATH finds another veles first",
            ),
            (
                PowerShell,
                &exe,
                &on_path,
                r".\veles.exe gateway restart".into(),
                "relative to a directory the cut shell had",
            ),
            (
                Cmd,
                &exe,
                &on_path,
                "veles gateway restart".into(),
                "cmd looks in the cut shell's directory before PATH",
            ),
            (
                PowerShell,
                &exe,
                &on_path,
                format!("& \"{quoted}\" gateway restart; Remove-Item x"),
                "the restart does not carry a write",
            ),
            (
                Posix,
                &plain_exe,
                &on_path,
                format!("{bare} gateway restart"),
                "to a POSIX shell the backslashes are escapes: another path",
            ),
        ] {
            assert_eq!(judge(dialect, this, path, &cmd), None, "{why}: {cmd}");
        }
    }

    /// A MODULE tool declares its own tier, and `acl::tool_cap` reads
    /// that declaration before the builtin table: one that declares a
    /// tier a guest holds (file reads) counts as read-only here, as it
    /// does for a guest; one that declares a writing tier goes to
    /// review, and so does a tool nothing classified.
    #[test]
    fn a_module_tool_is_judged_by_the_tier_it_declares() {
        crate::acl::declare_tool_cap("recovery_pin_module_reader", crate::acl::Cap::FileRead);
        crate::acl::declare_tool_cap("recovery_pin_module_writer", crate::acl::Cap::FileWrite);
        let none = serde_json::json!({});
        assert_eq!(
            cut_call("recovery_pin_module_reader", &none),
            Some(CutCall::ReadOnly)
        );
        assert_eq!(cut_call("recovery_pin_module_writer", &none), None);
        assert_eq!(cut_call("recovery_pin_undeclared_tool", &none), None);
        assert_eq!(cut_call("mcp__server__read_thing", &none), None);
    }
}
