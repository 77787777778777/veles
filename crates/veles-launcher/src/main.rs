//! `veles-gw`: start `veles gateway run` beside this binary with its
//! output in the gateway log, and exit. The Windows logon task runs
//! THIS (PLAN-windows.md §1): a console-subsystem `veles.exe` started
//! by the Task Scheduler would flash a console window at every logon,
//! and a GUI-subsystem `veles.exe` would break the CLI (PowerShell
//! returns at once from a GUI process and captures nothing — the
//! crow.exe trap in memory). So the split: `veles.exe` stays a console
//! program, this ~40-line GUI-subsystem program spawns it hidden.
//!
//! Usage: `veles-gw [<log file>]`. The log defaults to
//! `%LOCALAPPDATA%\veles\gateway.log` (`$XDG_DATA_HOME/veles` or
//! `~/.local/share/veles` elsewhere); `VELES_HOME` wins when set, as it
//! does for the gateway itself. No dependency on core: the launcher
//! must stay a single small file that links nothing the gateway links.
//!
//! It then SUPERVISES the gateway the way systemd's `Restart=on-failure`
//! and `StartLimitBurst` do on Linux: it waits, and a gateway that died
//! with a non-zero code (a crash, a kill) is started again after 3 s,
//! up to five times in two minutes, each restart noted in the log; a
//! clean `veles gateway stop` (exit 0) ends the launcher too, and five
//! deaths in two minutes make it give up with the last code. A restart
//! the gateway asked for (exit 76, the command center's Restart) is
//! started again the same way and is not a death. So the
//! logon task runs as long as the gateway does (`schtasks /Query` says
//! "Running"), and the restart does not rely on Task Scheduler:
//! measured 2026-10-13, its `RestartOnFailure` did not restart a task
//! started on demand whose action exited -1 (it covers a launch that
//! failed, and stays in the XML for that). Before this the task ended
//! the moment the launcher had spawned, and a dead gateway was dead
//! until the next logon.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The exit code of a restart the gateway asked for (the command
/// center's Restart): `veles_core::supervisor::RESTART_EXIT_CODE`, held
/// equal to this one by a core test, since the launcher links nothing
/// of core.
const RESTART_EXIT_CODE: i32 = 76;

fn default_log() -> PathBuf {
    if let Some(h) = std::env::var_os("VELES_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h).join("gateway.log");
    }
    if cfg!(windows) {
        if let Some(l) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(l).join("veles").join("gateway.log");
        }
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x).join("veles").join("gateway.log");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/veles/gateway.log")
}

fn main() {
    let log = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_log);
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .map(|d| d.join(if cfg!(windows) { "veles.exe" } else { "veles" }))
        .unwrap_or_else(|| PathBuf::from("veles"));
    let mut deaths: Vec<std::time::Instant> = Vec::new();
    loop {
        let (stdout, stderr) = log_handles(&log);
        let mut cmd = Command::new(&exe);
        // The gateway restarts itself by exiting non-zero when the
        // command center asks it to, and only under a supervisor known
        // to start it again (veles_core::supervisor): this is one.
        cmd.args(["gateway", "run"])
            .env("VELES_SUPERVISOR", "veles-gw")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: no console, and
            // a Ctrl-C meant for some console never reaches the gateway.
            cmd.creation_flags(0x0800_0000 | 0x0000_0200);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // The only place this program speaks: the log it was given.
                let _ = std::fs::write(
                    log.with_extension("launcher.err"),
                    format!("veles-gw: could not start {}: {e}\n", exe.display()),
                );
                std::process::exit(1);
            }
        };
        // The gateway's verdict: 0 for a clean stop, its own code
        // otherwise (1 when it died without one — a kill).
        let code = match child.wait() {
            Ok(st) => st.code().unwrap_or(1),
            Err(_) => 1,
        };
        match after_exit(&mut deaths, code, std::time::Instant::now()) {
            After::Stop => std::process::exit(0),
            After::GiveUp => {
                note(&log, &format!("gateway exited with code {code}: the fifth death in two minutes, giving up (`veles gateway restart` starts it again)"));
                std::process::exit(code);
            }
            After::Restart if code == RESTART_EXIT_CODE => note(
                &log,
                "the gateway asked to be restarted: starting it again in 3 s (not a death)",
            ),
            After::Restart => note(
                &log,
                &format!(
                    "gateway exited with code {code}: restarting in 3 s ({}/5 in two minutes)",
                    deaths.len()
                ),
            ),
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

/// What the launcher does once the gateway has exited.
#[derive(Debug, PartialEq, Eq)]
enum After {
    /// A clean stop: the launcher ends too.
    Stop,
    /// Start it again in 3 s.
    Restart,
    /// The fifth death in two minutes: end with its code.
    GiveUp,
}

/// The gateway exited with `code` at `now`; `deaths` are the deaths
/// counted so far, and this one is added when it counts. A restart the
/// gateway asked for ([`RESTART_EXIT_CODE`]) is started again and never
/// counted: the give-up is for a gateway that keeps dying.
fn after_exit(deaths: &mut Vec<std::time::Instant>, code: i32, now: std::time::Instant) -> After {
    if code == 0 {
        return After::Stop;
    }
    if code == RESTART_EXIT_CODE {
        return After::Restart;
    }
    deaths.retain(|t| now.duration_since(*t) < std::time::Duration::from_secs(120));
    deaths.push(now);
    if deaths.len() >= 5 {
        return After::GiveUp;
    }
    After::Restart
}

/// The gateway's stdout and stderr: the log, appended, or nothing.
fn log_handles(log: &std::path::Path) -> (Stdio, Stdio) {
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log);
    match out {
        Ok(f) => match f.try_clone() {
            Ok(g) => (Stdio::from(f), Stdio::from(g)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    }
}

/// One line from the launcher itself into the gateway log.
fn note(log: &std::path::Path, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(f, "veles-gw: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The command center's Restart is the gateway exiting with
    /// RESTART_EXIT_CODE so this launcher starts it again. Counted as a
    /// death, five Restarts in two minutes made the launcher give up and
    /// left a Windows node down until someone started it by hand. A
    /// requested restart is not a death; five real ones still end it.
    #[test]
    fn a_requested_restart_is_not_a_death() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut deaths = Vec::new();
        for i in 0..8 {
            assert_eq!(
                after_exit(&mut deaths, RESTART_EXIT_CODE, at(i * 12)),
                After::Restart,
                "restart {i}"
            );
        }
        assert!(deaths.is_empty(), "{deaths:?}");
        // Four crashes among the restarts, then a fifth: it gives up.
        for i in 0..4 {
            assert_eq!(after_exit(&mut deaths, 1, at(100 + i)), After::Restart);
            assert_eq!(
                after_exit(&mut deaths, RESTART_EXIT_CODE, at(100 + i)),
                After::Restart
            );
        }
        assert_eq!(after_exit(&mut deaths, -1, at(110)), After::GiveUp);
        // Deaths older than two minutes are forgotten; a clean stop ends.
        let mut deaths = vec![at(0), at(1), at(2), at(3)];
        assert_eq!(after_exit(&mut deaths, 1, at(200)), After::Restart);
        assert_eq!(after_exit(&mut deaths, 0, at(201)), After::Stop);
    }
}
