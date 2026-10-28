//! Background shells — commands that outlive the tool call that started
//! them.
//!
//! WHAT WAS HERE BEFORE: nothing. `TerminalTool` runs `bash -c` and
//! awaits completion under a timeout; there was no background flag, no
//! handle, no poll path and no kill. The two `/background` commands that
//! looked like this feature ran an extra AGENT TURN and discarded its
//! output (PENDING.md, 2026-09-02 audit).
//!
//! THE MODEL IS `lsp_proxy.rs`. That is the one place in this tree that
//! already keeps a child process alive across calls and gets it right:
//! `kill_on_drop` so a dropped session reaps rather than leaks, a
//! `try_wait` liveness check, and a hard cap on buffered bytes. The only
//! thing it lacks is a map — it holds a single session. This module is
//! that design with an id-keyed map.
//!
//! OWNERSHIP. One task per shell owns the `Child` and races
//! `child.wait()` against the cancel token, so nothing else needs a
//! `Mutex<Child>`: kill is "flip the token", exactly like every other
//! task kind. Output is pumped into a bounded ring by two reader tasks
//! and read back by id.
//!
//! COMPLETION reaches the model at the next turn boundary through the
//! existing background-event queue (`delegate.rs`), rendered as
//! `[BACKGROUND SHELL COMPLETE]`. As with background subagents, an idle
//! session surfaces nothing until it takes another turn — there is no
//! path into a turn loop that is not running.

use crate::lockutil::LockOk;
use crate::providers::CancelToken;
use crate::tasks::{TaskKind, TaskStatus};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Output lines retained per shell. A `yes`-style runaway must not eat
/// memory until the process is OOM-killed.
const MAX_LINES: usize = 500;
/// Per-line cap, so one enormous line cannot defeat MAX_LINES.
const MAX_LINE_BYTES: usize = 4_000;
/// How often the waiter checks the cancel token.
const POLL_MS: u64 = 100;

struct ShellSession {
    command: String,
    out: Arc<Mutex<VecDeque<String>>>,
    /// `None` while running; `Some(code)` once exited (-1 for a signal).
    exit: Arc<Mutex<Option<i32>>>,
    /// The box it runs in, by name; `None` on the host.
    sandbox: Option<String>,
}

fn sessions() -> &'static Mutex<HashMap<u32, ShellSession>> {
    static S: std::sync::OnceLock<Mutex<HashMap<u32, ShellSession>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn push_line(ring: &Arc<Mutex<VecDeque<String>>>, line: String) {
    let mut line = line;
    if line.len() > MAX_LINE_BYTES {
        // Cut on a char boundary — a byte slice would panic on UTF-8.
        let cut = line
            .char_indices()
            .take_while(|(i, _)| *i < MAX_LINE_BYTES)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        line.truncate(cut);
        line.push_str(" …[line truncated]");
    }
    let mut r = ring.lock_ok();
    if r.len() >= MAX_LINES {
        r.pop_front();
    }
    r.push_back(line);
}

/// Start `command` in the background. Returns the task id it registered
/// under — the same id space as subagents and monitors, so `/tasks`
/// lists all three and one cancel path serves all three.
///
/// `sandbox_arg` is what the call said about its zone (terminal's
/// `host` / `sandbox` arguments); the ZONE itself is resolved from the session
/// principal inside — below admin it is ALWAYS the caller's env
/// container, flag or no flag (user_envs owns that invariant).
pub fn start(command: &str, session_id: &str, sandbox_arg: crate::user_envs::ZoneArg) -> u32 {
    let (id, cancel) = crate::tasks::start(TaskKind::Shell, command, session_id);

    let out: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let exit: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    let zone = crate::user_envs::zone_for(Some(session_id), sandbox_arg);

    sessions().lock_ok().insert(
        id,
        ShellSession {
            command: command.to_string(),
            out: out.clone(),
            exit: exit.clone(),
            sandbox: match &zone {
                crate::user_envs::Zone::Sandbox { name, .. } => Some(name.clone()),
                crate::user_envs::Zone::Host => None,
            },
        },
    );

    let cmd = command.to_string();
    let parent = session_id.to_string();
    let parent_for_run = parent.clone();
    crate::bg::spawn(async move {
        let ok = match zone {
            crate::user_envs::Zone::Sandbox { key, name: _ } => {
                run_in_env(id, &key, &cmd, cancel, out.clone(), exit.clone()).await
            }
            crate::user_envs::Zone::Host => {
                run(id, &cmd, &parent_for_run, cancel, out.clone(), exit.clone()).await
            }
        };
        crate::tasks::finish(id, ok);
        // The completion half, shared with `adopt` — queue entry first,
        // bus event second (see `finish_plumbing`).
        finish_plumbing(id, &cmd, &parent, &out, &exit).await;
    });

    id
}

/// The ring type every shell's captured output lives in.
pub type Ring = Arc<Mutex<VecDeque<String>>>;
/// The exit slot: `None` while running, `Some(code)` once exited
/// (-1 for a signal / lost runner).
pub type ExitSlot = Arc<Mutex<Option<i32>>>;

/// ADOPT a live foreground command into the background-shell registry —
/// the fg→bg move (USER taxonomy, 2026-09-17: "there should also be a
/// way for the user to move a running foreground shell into the
/// background").
///
/// Unlike `start`, this spawns nothing. The `terminal` tool already has
/// the command running under a pump task ON THE bg RUNTIME (process-
/// wide, so it outlives any turn-local runtime), feeding a ring of
/// scrubbed lines and an exit slot from the moment it spawned — that is
/// what makes the handover trivial AND complete: the ring already holds
/// the full history, pre-detach output included, with no flush race.
/// Detach registers the row around those existing Arcs; the pump then
/// polls the returned cancel token like any born-background shell and
/// calls [`adopted_finish`] at exit, so an adopted shell is
/// indistinguishable from a started one to every surface: same id
/// space, same [output]/[kill], same [BACKGROUND SHELL COMPLETE]
/// injection and shell-complete bus event.
pub fn adopt_live(
    command: &str,
    session_id: &str,
    ring: Ring,
    exit: ExitSlot,
) -> (u32, CancelToken) {
    let (id, cancel) = crate::tasks::start(TaskKind::Shell, command, session_id);
    sessions().lock_ok().insert(
        id,
        ShellSession {
            command: command.to_string(),
            out: ring,
            exit,
            // Only a host-zone foreground command is adopted: a box's
            // commands run through `call_in_env`, which has no pump.
            sandbox: None,
        },
    );
    (id, cancel)
}

/// Append one line to a shell ring with the registry's bounds
/// (MAX_LINES × MAX_LINE_BYTES) — `push_line`, exposed for the
/// `terminal` tool's pump so an adopted shell's ring is capped exactly
/// like a born-background one.
pub fn push_capped(ring: &Ring, line: String) {
    push_line(ring, line);
}

/// An adopted command has exited: finish the task row and run the SAME
/// completion plumbing every shell shares (delegate queue first, bus
/// event second). `ok` is `code == 0`; a cancelled token makes the row
/// Cancelled regardless, exactly as `tasks::finish` does for every
/// other shell. Called by the pump task, which is already on the bg
/// runtime.
pub async fn adopted_finish(
    id: u32,
    command: &str,
    parent: &str,
    ring: &Ring,
    exit: &ExitSlot,
    ok: bool,
) {
    crate::tasks::finish(id, ok);
    finish_plumbing(id, command, parent, ring, exit).await;
}

/// The completion half every shell path shares: delegate-queue entry
/// FIRST, then the settle-and-publish of the shell-complete bus event
/// (the wake loop reads `has_background_events` on receipt, so the
/// order is load-bearing). Factored out of `start`'s spawn block so
/// `adopt` cannot drift from it.
async fn finish_plumbing(
    id: u32,
    cmd: &str,
    parent: &str,
    out: &Arc<Mutex<VecDeque<String>>>,
    exit: &Arc<Mutex<Option<i32>>>,
) {
    let status = crate::tasks::get(id)
        .map(|t| t.status)
        .unwrap_or(TaskStatus::Done);
    let tail: Vec<String> = out.lock_ok().iter().rev().take(40).rev().cloned().collect();
    crate::delegate::push_background_event(
        parent,
        crate::delegate::BackgroundEvent {
            child_id: id,
            kind: TaskKind::Shell,
            goal: cmd.to_string(),
            result: json!({
                "status": status.as_str(),
                "exit_code": *exit.lock_ok(),
                "summary": tail.join("\n"),
            }),
            child_session: None,
            finished_at: std::time::SystemTime::now(),
        },
    );

    // Output pumps may still be draining the pipes the instant the
    // process exits; settle briefly so the on-the-wire tail is real
    // rather than a snapshot taken a scheduling instant too early.
    for _ in 0..8 {
        if !out.lock_ok().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let tail_lines: Vec<String> = out.lock_ok().iter().rev().take(4).rev().cloned().collect();
    let mut tail = tail_lines.join("\n");
    if tail.len() > 1200 {
        // Cut on a char boundary — a byte slice would panic on UTF-8
        // (same discipline as push_line).
        let cut = tail
            .char_indices()
            .take_while(|(i, _)| *i < 1200)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        tail.truncate(cut);
        tail.push_str(" …");
    }
    crate::session_tap::publish(
        parent,
        "shell-complete",
        json!({
            "session_id": parent,
            "id": id,
            "command": cmd,
            "exit_code": *exit.lock_ok(),
            "tail": tail,
        }),
    );
}

/// Spawn, pump both pipes into the ring, and race the wait against the
/// cancel token. Returns whether the command exited 0.
async fn run(
    id: u32,
    command: &str,
    session_id: &str,
    cancel: CancelToken,
    out: Arc<Mutex<VecDeque<String>>>,
    exit: Arc<Mutex<Option<i32>>>,
) -> bool {
    // Policy and principal decide, as for `terminal` (see
    // `secrets::child_env`).
    let child = crate::secrets::child_env(session_id, command);
    let refs = child.inject.clone();

    let mut c = crate::shell::command(command);
    c.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        // Without this a dropped Child detaches and the whole process
        // tree outlives us. `terminal` has had that bug all along.
        .kill_on_drop(true);
    // Give the child a tree of its own BEFORE spawning. Killing only the
    // runner would leave a backgrounded compiler, server, or test binary
    // alive and reparented after the shell row said "cancelled".
    crate::platform::own_group(&mut c);
    // F3: same strip as `terminal` — a non-owner principal's background
    // shell inherits no secret-bearing env, and an owner's loses the
    // engine-only credentials. Referenced $NAME values are injected
    // after, so explicit use still works.
    for k in &child.strip {
        c.env_remove(k);
    }
    if let Some(n) = child.note() {
        push_line(&out, n);
    }
    for (k, v) in &refs {
        c.env(k, v);
    }

    let child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            push_line(&out, format!("error: {e}"));
            *exit.lock_ok() = Some(-1);
            return false;
        }
    };

    let tree = child.id().and_then(crate::platform::adopt);
    pump_and_wait(id, child, tree, cancel, out, exit).await
}

/// Wait for a born-background shell and pump its pipes into the bounded,
/// scrubbed ring. `tree` is the platform process group or Job Object
/// adopted immediately after spawn; cancellation kills that tree, not
/// merely the shell's immediate child. Adopted foreground shells keep
/// using the terminal tool's existing tree and cancel watch instead.
async fn pump_and_wait(
    id: u32,
    mut child: tokio::process::Child,
    tree: Option<crate::platform::Tree>,
    cancel: CancelToken,
    out: Arc<Mutex<VecDeque<String>>>,
    exit: Arc<Mutex<Option<i32>>>,
) -> bool {
    // Scrub every known secret value out of captured output before it
    // is stored, the same guarantee `terminal` gives. A DIFFERENT map
    // from the injection one: `load` is "what may $NAME expand to",
    // which is every .env key, and scrubbing with that redacted the
    // server hostname out of ordinary output.
    let scrub_with = std::sync::Arc::new(crate::secrets::scrubbable());

    if let Some(so) = child.stdout.take() {
        let ring = out.clone();
        let sec = scrub_with.clone();
        crate::rt::spawn(async move {
            let mut lines = BufReader::new(so).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                push_line(&ring, crate::secrets::scrub(&l, &sec));
            }
        });
    }
    if let Some(se) = child.stderr.take() {
        let ring = out.clone();
        let sec = scrub_with.clone();
        crate::rt::spawn(async move {
            let mut lines = BufReader::new(se).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                push_line(&ring, crate::secrets::scrub(&l, &sec));
            }
        });
    }

    // Cancellation is cooperative everywhere else in this tree, so it is
    // here too: poll the flag and translate it into a real kill. That is
    // the same shape delegate.rs uses for subagent children.
    let code = loop {
        if cancel.is_cancelled() {
            if let Some(t) = &tree {
                t.kill();
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            push_line(&out, "[cancelled]".to_string());
            break -1;
        }
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await,
            Err(e) => {
                push_line(&out, format!("error: {e}"));
                break -1;
            }
        }
    };

    *exit.lock_ok() = Some(code);
    tracing::info!("background shell {id} exited {code}");
    code == 0
}

/// The sandbox-zone counterpart of [`run`]: the command executes
/// inside the principal's env container via a framework-staged runner
/// script (`user_envs::bg_start`), output and exit code flow back
/// through workspace files the waiter polls HOST-side — every read
/// behind the stays-under-workspace guard, because a symlink planted
/// at `.veles-bg/<id>/out` must not turn a poll into a host read.
///
/// Liveness is re-probed in the container every few seconds: a runner
/// that dies without writing its code file (container restarted,
/// process reaped) is reported lost rather than waited on forever.
async fn run_in_env(
    id: u32,
    key: &str,
    command: &str,
    cancel: CancelToken,
    out: Arc<Mutex<VecDeque<String>>>,
    exit: Arc<Mutex<Option<i32>>>,
) -> bool {
    let scrub_with = std::sync::Arc::new(crate::secrets::scrubbable());
    let workdir = match crate::user_envs::ensure(key).await {
        Ok(w) => w,
        Err(e) => {
            push_line(&out, format!("error: user env {key}: {e:#}"));
            *exit.lock_ok() = Some(-1);
            return false;
        }
    };
    let pid = match crate::user_envs::bg_start(key, id, command, &workdir).await {
        Ok(p) => p,
        Err(e) => {
            push_line(&out, format!("error: user env {key}: {e:#}"));
            *exit.lock_ok() = Some(-1);
            return false;
        }
    };

    let mut cursor = 0usize;
    let mut last_probe = std::time::Instant::now();
    // Under a remote podman there is no bind mount to read, and every
    // poll is a `podman exec`: once a second, not every tick. The same in
    // the userland, whose boxes are another app's files (a poll is a ulsh
    // session there).
    let remote = crate::user_envs::polls_by_exec();
    let poll_ms = if remote { 1000 } else { POLL_MS };
    let code = loop {
        if cancel.is_cancelled() {
            let _ = crate::user_envs::bg_kill(key, pid).await;
            push_line(&out, "[cancelled]".to_string());
            break -1;
        }
        let (chunk, next, code) = if remote {
            crate::user_envs::bg_poll_remote(key, id, cursor).await
        } else {
            crate::user_envs::bg_poll(&workdir, id, cursor)
        };
        cursor = next;
        for line in chunk.lines() {
            if !line.is_empty() {
                push_line(&out, crate::secrets::scrub(line, &scrub_with));
            }
        }
        if let Some(c) = code {
            break c;
        }
        // Every ~5s confirm the runner is still alive IN the container
        // (each probe is a podman exec — too costly per 100ms tick).
        if last_probe.elapsed().as_secs() >= 5 {
            last_probe = std::time::Instant::now();
            match crate::user_envs::bg_state(key, id, pid).await {
                Ok("alive") | Ok("done") => {}
                Ok("dead") => {
                    // One last poll in case the code file landed between
                    // the probe request and now.
                    let (_, _, code) = if remote {
                        crate::user_envs::bg_poll_remote(key, id, cursor).await
                    } else {
                        crate::user_envs::bg_poll(&workdir, id, cursor)
                    };
                    break code.unwrap_or(-2);
                }
                Ok(_) => unreachable!("bg_state returns alive|dead|done"),
                Err(e) => {
                    push_line(&out, format!("warning: env probe failed: {e:#}"));
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    };
    if code == -2 {
        push_line(
            &out,
            "[sandbox] runner lost without an exit code (container restarted?)".to_string(),
        );
    }
    *exit.lock_ok() = Some(code);
    tracing::info!("background shell {id} exited {code} (env {key})");
    code == 0
}

/// Captured output so far, oldest first. `None` for an unknown id.
pub fn output(id: u32) -> Option<String> {
    let s = sessions().lock_ok();
    let sess = s.get(&id)?;
    let lines: Vec<String> = sess.out.lock_ok().iter().cloned().collect();
    Some(lines.join("\n"))
}

/// May a call made from `asking` reach a shell owned by `owned_by`?
///
/// A background shell's id is a small integer in a PROCESS-wide space,
/// and `detail`/`kill` took it bare — so one gateway session could read
/// another's captured output (a host-zone shell the owner started, say)
/// or kill its process, by guessing a number. Same shape as the ask
/// binding in `ask.rs`: your own, or you are the owner.
///
/// No principal on the asking session means an owner surface (REPL,
/// TUI, desktop), which keeps the whole-process view it always had —
/// one human, many sessions.
pub fn may_reach(asking: &str, owned_by: &str) -> bool {
    if asking == owned_by {
        return true;
    }
    crate::acl::session_principal(asking)
        .map(|p| p.has(crate::acl::Cap::All))
        .unwrap_or(true)
}

/// `detail`, scoped to what the calling session may reach.
pub fn detail_for(id: u32, session_id: &str) -> Option<Value> {
    let rec = crate::tasks::get(id)?;
    if !may_reach(session_id, &rec.session) {
        return None;
    }
    detail(id)
}

/// `list`, scoped to what the calling session may reach.
pub fn list_for(session_id: &str) -> Vec<Value> {
    let owners: HashMap<u32, String> = crate::tasks::all()
        .into_iter()
        .map(|t| (t.id, t.session))
        .collect();
    list()
        .into_iter()
        .filter(|v| {
            v["id"]
                .as_u64()
                .map(|id| id as u32)
                .and_then(|id| owners.get(&id).cloned())
                .map(|owner| may_reach(session_id, &owner))
                // A shell whose task record has vanished is nobody's to
                // show.
                .unwrap_or(false)
        })
        .collect()
}

/// `kill`, scoped to what the calling session may reach.
pub fn kill_for(id: u32, session_id: &str) -> bool {
    match crate::tasks::get(id) {
        Some(rec) if may_reach(session_id, &rec.session) => kill(id),
        _ => false,
    }
}

/// Structured detail for the API and the `shell` tool.
pub fn detail(id: u32) -> Option<Value> {
    let rec = crate::tasks::get(id)?;
    let s = sessions().lock_ok();
    let sess = s.get(&id)?;
    let lines: Vec<String> = sess.out.lock_ok().iter().cloned().collect();
    Some(json!({
        "id": id,
        "command": sess.command,
        "sandbox": sess.sandbox,
        "status": rec.status.as_str(),
        "exit_code": *sess.exit.lock_ok(),
        "elapsed_s": rec.elapsed_secs(),
        "lines": lines.len(),
        "output": lines.join("\n"),
    }))
}

/// Every shell this process started, running or not.
pub fn list() -> Vec<Value> {
    let s = sessions().lock_ok();
    let mut ids: Vec<u32> = s.keys().copied().collect();
    ids.sort_unstable();
    ids.iter()
        .filter_map(|id| {
            let sess = s.get(id)?;
            let rec = crate::tasks::get(*id);
            Some(json!({
                "id": id,
                "command": sess.command,
                "status": rec.as_ref().map(|r| r.status.as_str()).unwrap_or("unknown"),
                "exit_code": *sess.exit.lock_ok(),
                "elapsed_s": rec.as_ref().map(|r| r.elapsed_secs()).unwrap_or(0),
            }))
        })
        .collect()
}

/// Ask a running shell to stop. The waiter notices within POLL_MS and
/// kills the process; `false` means unknown or already finished.
pub fn kill(id: u32) -> bool {
    crate::tasks::cancel(id)
}

/// The `/shells` command — the registry `start` has always kept, now
/// reachable from a chat surface. `list` shows every shell this
/// process started (running or finished — a finished shell is exactly
/// the one whose OUTPUT you want to read), scoped to what the calling
/// session may reach; `output <id>` the captured text; `kill <id>`
/// stops a running one. `/tasks` overlaps (it lists live tasks of
/// every kind); this family answers "what did that background shell
/// do", including after it finished.
pub fn command(rest: &str, session_id: &str) -> String {
    let mut parts = rest.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("output"), Some(id)) => match id
            .parse::<u32>()
            .ok()
            .and_then(|i| detail_for(i, session_id))
        {
            None => format!("no shell {id} reachable from this session"),
            Some(v) => {
                let out = v["output"].as_str().unwrap_or("");
                if out.is_empty() {
                    format!("shell {id}: (no output captured yet)")
                } else {
                    format!("shell {id} output:\n{out}")
                }
            }
        },
        (Some("kill") | Some("cancel"), Some(id)) => match id.parse::<u32>() {
            Ok(i) if kill_for(i, session_id) => format!("shell {i} asked to stop"),
            Ok(i) => format!("shell {i} is not running, or is not reachable from this session"),
            Err(_) => "usage: /shells kill <id>".into(),
        },
        (Some(sub), _) => {
            format!("unknown /shells subcommand '{sub}' — try list | output <id> | kill <id>")
        }
        (None, _) => {
            let shells = list_for(session_id);
            if shells.is_empty() {
                return "no shells — /shells shows ones a background shell call started".into();
            }
            let mut s = String::from("background shells:\n");
            for v in &shells {
                s.push_str(&format!(
                    "  [{}] {:<7} {}\n",
                    v["id"].as_u64().unwrap_or(0),
                    v["status"].as_str().unwrap_or("?"),
                    v["command"].as_str().unwrap_or("?")
                ));
            }
            s.push_str("/shells output <id> · /shells kill <id>");
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These drive real processes, so they need a runtime and they only
    // assert things that are true regardless of machine speed.

    #[tokio::test]
    async fn a_shell_runs_captures_output_and_exits() {
        let id = start(
            "echo hello-from-shell; exit 0",
            "sess-t1",
            crate::user_envs::ZoneArg::Unset,
        );
        for _ in 0..100 {
            if crate::tasks::get(id).map(|t| t.status) != Some(TaskStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let d = detail(id).expect("shell is registered");
        assert_eq!(d["status"], "done", "detail: {d}");
        assert_eq!(d["exit_code"], 0);
        assert!(
            d["output"].as_str().unwrap().contains("hello-from-shell"),
            "output: {d}"
        );
    }

    /// A background shell belongs to the session that started it. Shell
    /// ids are small integers in one process-wide space, and `detail`
    /// and `kill` took them bare — so a gateway guest could read the
    /// captured output of a HOST-zone shell the owner started, or kill
    /// it, by guessing a number. Scoping is by session, with the owner
    /// (and every owner surface, which has no principal) keeping the
    /// whole-process view.
    #[tokio::test]
    async fn a_shell_belongs_to_the_session_that_started_it() {
        let owner_sid = "sess-owner-shell";
        let guest_sid = "sess-guest-shell";
        crate::acl::set_session_principal(
            guest_sid,
            crate::acl::Principal::known("irc:eve", crate::acl::Role::User),
        );
        let wait = if cfg!(windows) {
            "Write-Output owner-secret-output; Start-Sleep 30"
        } else {
            "echo owner-secret-output; sleep 30"
        };
        let id = start(wait, owner_sid, crate::user_envs::ZoneArg::Unset);
        // Give the reader tasks a moment to capture the echo.
        for _ in 0..40 {
            if output(id).is_some_and(|o| o.contains("owner-secret-output")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // The owning session sees it…
        assert!(
            detail_for(id, owner_sid).is_some(),
            "the starting session must see its own shell"
        );
        assert!(list_for(owner_sid).iter().any(|v| v["id"] == id));

        // …the guest sees nothing, reads nothing, and kills nothing.
        assert!(
            detail_for(id, guest_sid).is_none(),
            "another session's shell must read as 'no such shell'"
        );
        assert!(
            !list_for(guest_sid).iter().any(|v| v["id"] == id),
            "another session's shell must not be listed"
        );
        assert!(
            !kill_for(id, guest_sid),
            "another session must not be able to kill it"
        );
        assert_eq!(
            crate::tasks::get(id).map(|t| t.status),
            Some(TaskStatus::Running),
            "the shell must still be running after the refused kill"
        );

        // The owner can still stop it — scoping is not a lockout.
        assert!(kill_for(id, owner_sid));
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_an_error_not_a_cancel() {
        let id = start("exit 3", "sess-t2", crate::user_envs::ZoneArg::Unset);
        for _ in 0..100 {
            if crate::tasks::get(id).map(|t| t.status) != Some(TaskStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(crate::tasks::get(id).unwrap().status, TaskStatus::Error);
        assert_eq!(detail(id).unwrap()["exit_code"], 3);
    }

    /// The assertion that matters: cancel must actually stop the
    /// process, not merely relabel the row. (a2a's tasks/cancel flips a
    /// state string and leaves the work running — that is the failure
    /// mode this test exists to exclude.)
    #[tokio::test]
    async fn kill_stops_a_long_runner_and_reports_cancelled() {
        let wait = if cfg!(windows) {
            "Start-Sleep 300"
        } else {
            "sleep 300"
        };
        let id = start(wait, "sess-t3", crate::user_envs::ZoneArg::Unset);
        // Let it actually get going.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(crate::tasks::get(id).unwrap().status, TaskStatus::Running);

        assert!(kill(id));
        for _ in 0..100 {
            if crate::tasks::get(id).map(|t| t.status) != Some(TaskStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            crate::tasks::get(id).unwrap().status,
            TaskStatus::Cancelled,
            "a killed shell reports cancelled, not error"
        );
    }

    #[tokio::test]
    async fn output_ring_is_bounded() {
        let id = start(
            &format!("seq 1 {}", MAX_LINES * 3),
            "sess-t4",
            crate::user_envs::ZoneArg::Unset,
        );
        for _ in 0..200 {
            if crate::tasks::get(id).map(|t| t.status) != Some(TaskStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let d = detail(id).unwrap();
        assert!(
            d["lines"].as_u64().unwrap() <= MAX_LINES as u64,
            "ring grew past the cap: {}",
            d["lines"]
        );
    }

    /// The canary belongs to a detached grandchild, not to the shell
    /// itself. It must never appear after the background task is killed.
    #[tokio::test]
    async fn killing_a_shell_ends_its_detached_process_tree() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let canary = dir.path().join("survivor");
        #[cfg(unix)]
        let cmd = format!(
            "(sleep 2; printf survived > {}) </dev/null >/dev/null 2>&1 & wait",
            canary.display()
        );
        #[cfg(windows)]
        let cmd = format!(
            "Start-Process -WindowStyle Hidden {} -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep 2; Set-Content -NoNewline -Value survived -Path ''{}'''; Start-Sleep 30",
            crate::shell::shell_binary(),
            canary.display()
        );
        let id = start(&cmd, "sess-tree-cancel", crate::user_envs::ZoneArg::Unset);
        // Wait until the runner has actually spawned its detached child,
        // rather than cancelling before the command reaches that line.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(kill(id));
        for _ in 0..100 {
            if crate::tasks::get(id).map(|t| t.status) != Some(TaskStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            crate::tasks::get(id).map(|t| t.status),
            Some(TaskStatus::Cancelled)
        );
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        assert!(
            !canary.exists(),
            "a detached child survived cancellation and wrote its canary"
        );
    }

    #[test]
    fn unknown_ids_answer_none_not_panic() {
        assert!(output(999_999).is_none());
        assert!(detail(999_999).is_none());
        assert!(!kill(999_999));
    }

    /// The session bus learns a shell finished the moment it happens —
    /// not at the owning session's next turn boundary. This is the
    /// surface-visibility half of the contract; the model's
    /// turn-boundary [BACKGROUND SHELL COMPLETE] injection is separate
    /// and unchanged (an idle session has no turn to wake).
    #[tokio::test]
    async fn a_shell_completion_reaches_the_session_bus_immediately() {
        let sid = "shell-complete-pin";
        let mut rx = crate::session_tap::subscribe();
        // Subscribe BEFORE starting: the bus is process-global and
        // publish is fire-and-forget, so a late subscriber sees nothing.
        let id = start("exit 0", sid, crate::user_envs::ZoneArg::Unset);
        // Filter other tests' traffic the way real consumers do.
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let e = rx.recv().await.expect("bus closed");
                if e.session_id == sid {
                    break e;
                }
            }
        })
        .await
        .expect("no shell-complete event within 5s");
        assert_eq!(
            ev.kind, "shell-complete",
            "the bus event names the completion"
        );
        assert_eq!(ev.data["id"], id, "the event carries the shell's id");
        assert!(
            ev.data["command"].as_str().unwrap_or("").contains("exit 0"),
            "the event names the command: {ev:?}"
        );
    }

    /// The shared `/shells` body: what the surface command dispatches
    /// to. Like the `/tasks` command tests, these paths stay off the
    /// global registry for the cases that must not race instance-based
    /// tests — only unknown ids and bogus subcommands run here.
    #[test]
    fn command_parses_subcommands_and_refuses_unknown_ids() {
        let sid = "sess-shells-cmd";
        assert!(
            command("output abc", sid).contains("no shell abc"),
            "a non-numeric id is refused, not parsed as zero"
        );
        assert!(
            command("output 999999", sid).contains("no shell 999999"),
            "an unknown id names the id"
        );
        assert!(
            command("kill abc", sid).contains("usage: /shells kill"),
            "kill with a bad id carries the usage line"
        );
        assert!(
            command("frobnicate", sid).contains("unknown /shells subcommand"),
            "an unknown verb is named, not silently listed"
        );
        // ONE snapshot: the listing is process-global state read at
        // command time, so a single call can honestly be either the
        // empty-table or the table shape — asserting on two separate
        // calls races parallel tests.
        let list = command("", sid);
        assert!(
            list.contains("background shells") || list.contains("no shells"),
            "no argument lists the table (possibly empty): {list}"
        );
    }

    /// `/shells` sits at the same guest capability floor as `/tasks`,
    /// and shell ids are process-wide — scoping must ride the command
    /// too, not only the bare registry functions. This uses OWNED rows
    /// so it deliberately touches the shared registry: one shell, held
    /// by `sess-owner-shells-cmd`, must be invisible to a guest
    /// session's `/shells list` and unkillable through it.
    #[tokio::test]
    async fn the_shells_command_is_scoped_to_the_calling_session() {
        let owner_sid = "sess-owner-shells-cmd";
        let guest_sid = "sess-guest-shells-cmd";
        crate::acl::set_session_principal(
            guest_sid,
            crate::acl::Principal::known("irc:eve", crate::acl::Role::User),
        );
        let wait = if cfg!(windows) {
            "Start-Sleep 300"
        } else {
            "sleep 300"
        };
        let id = start(wait, owner_sid, crate::user_envs::ZoneArg::Unset);

        let list = command("", guest_sid);
        assert!(
            !list.contains(&format!("[{id}]")),
            "a stranger's shell must not appear in the guest's list: {list}"
        );
        assert!(
            command(&format!("kill {id}"), guest_sid).contains("not reachable"),
            "a stranger's session must not kill it through /shells"
        );
        assert_eq!(
            crate::tasks::get(id).map(|t| t.status),
            Some(TaskStatus::Running),
            "the refused kill must leave the shell running"
        );

        // The owner's view shows it, and the owner can stop it.
        assert!(
            command("", owner_sid).contains(&format!("[{id}]")),
            "the owner's list shows the shell"
        );
        assert!(
            kill_for(id, owner_sid),
            "the owner may stop it — scoping is not a lockout"
        );
    }

    /// The command's `output` answers with the CAPTURED text — the
    /// whole point of `/shells` over `/tasks`: a finished shell's
    /// output stays readable after the task row has moved past
    /// "running". Driver: a shell that echoes a marker and exits; the
    /// command lies downstream of `detail_for`, so this pins the
    /// composed path (command string → registry → ring buffer → text).
    #[tokio::test]
    async fn the_command_reads_a_finished_shells_captured_output() {
        let sid = "sess-shells-output";
        let id = start(
            "echo marker-output-abc",
            sid,
            crate::user_envs::ZoneArg::Unset,
        );
        // Wait for the shell to actually run: the registry row flips to
        // Done when the exit flips.
        for _ in 0..100 {
            if crate::tasks::get(id).map(|t| t.status) != Some(TaskStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let text = command(&format!("output {id}"), sid);
        assert!(
            text.contains("marker-output-abc"),
            "captured output reaches /shells output: {text}"
        );

        // And an empty-ish non-marker id is refused with the id named.
        assert!(
            command("output 999999", sid).contains("no shell 999999"),
            "unknown ids stay refused with the id named"
        );
    }
}
