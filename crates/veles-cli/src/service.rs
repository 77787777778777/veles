//! The service manager behind `veles gateway install / uninstall /
//! status / stop / restart`: a systemd user unit on unix, a Task
//! Scheduler logon task on Windows (PLAN-windows.md §1; USER
//! 2026-10-13: the logon task now, a Windows Service later). The
//! commands in `lib.rs` ask this module first and fall back to the pid
//! file when no manager runs the gateway.
//!
//! The Windows task runs `veles-gw.exe` (crates/veles-launcher), not
//! `veles.exe`: a console program started by the scheduler flashes a
//! console window at logon; the launcher is a GUI-subsystem program
//! that spawns the gateway hidden and exits.

use std::path::PathBuf;

/// The task's / unit's name.
pub const NAME: &str = "veles-gateway";

/// Whether this platform's manager has the gateway installed.
pub fn installed() -> bool {
    imp::installed()
}

/// One status line for `veles gateway status`, and whether the manager
/// reports it active.
pub fn status() -> (String, bool) {
    imp::status()
}

/// Stop through the manager. `None` when it is not installed (the
/// caller then stops by pid).
pub fn stop() -> Option<anyhow::Result<()>> {
    installed().then(imp::stop)
}

/// Restart through the manager. `None` when it is not installed.
pub fn restart() -> Option<anyhow::Result<()>> {
    installed().then(imp::restart)
}

/// Install and start.
pub fn install() -> anyhow::Result<()> {
    imp::install()
}

/// Stop, disable and remove.
pub fn uninstall() -> anyhow::Result<()> {
    imp::uninstall()
}

#[cfg(unix)]
mod imp {
    use super::NAME;
    use crate::veles_core;
    use std::path::PathBuf;

    fn unit_dir() -> anyhow::Result<PathBuf> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| veles_core::config::user_home().join(".config"));
        let dir = base.join("systemd/user");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn unit_path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| veles_core::config::user_home().join(".config"));
        base.join(format!("systemd/user/{NAME}.service"))
    }

    pub fn installed() -> bool {
        veles_core::lifecycle::has_systemd_unit()
    }

    pub fn status() -> (String, bool) {
        if !installed() {
            return (
                "  systemd: no unit installed (`veles gateway install` to add)".into(),
                false,
            );
        }
        match std::process::Command::new("systemctl")
            .args(["--user", "is-active", NAME])
            .output()
        {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                (format!("  systemd: {NAME}.service -> {s}"), s == "active")
            }
            Err(_) => (
                "  systemd: unit present, but systemctl unavailable".into(),
                false,
            ),
        }
    }

    fn systemctl(verb: &str) -> anyhow::Result<()> {
        let st = std::process::Command::new("systemctl")
            .args(["--user", verb, NAME])
            .status();
        match st {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => anyhow::bail!("systemctl {verb} failed (exit {s})"),
            Err(e) => anyhow::bail!("systemctl {verb} failed: {e}"),
        }
    }

    pub fn stop() -> anyhow::Result<()> {
        systemctl("stop")?;
        println!("veles gateway: stopped (systemd)");
        Ok(())
    }

    pub fn restart() -> anyhow::Result<()> {
        systemctl("restart")?;
        println!("veles gateway: restarted (systemd)");
        Ok(())
    }

    /// Write a systemd user unit that runs `veles gateway run` as a
    /// service, then enable + start it. The unit reads secrets from
    /// the config dir's `.env` (the same file the gateway loads) and
    /// relies on systemd's own crash-loop brake (Restart=on-failure +
    /// StartLimitBurst).
    pub fn install() -> anyhow::Result<()> {
        let exe = std::env::current_exe()?;
        let dir = unit_dir()?;
        let unit = dir.join(format!("{NAME}.service"));
        // The .env is in the CONFIG dir; a unit pointing EnvironmentFile
        // at the data home starts a gateway with none of its secrets.
        let env_dir = veles_core::config::config_dir();
        let unit_body = format!(
            "[Unit]\n\
             Description=Veles Agent Gateway\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             StartLimitIntervalSec=120\n\
             StartLimitBurst=5\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={} gateway run\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             EnvironmentFile=-{}/.env\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            exe.display(),
            env_dir.display()
        );
        std::fs::write(&unit, unit_body)?;
        println!("wrote {unit:?}");

        let reload = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()?;
        if !reload.success() {
            anyhow::bail!("systemctl daemon-reload failed (exit {reload})");
        }
        let enable = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", NAME])
            .status()?;
        if !enable.success() {
            anyhow::bail!("systemctl enable --now failed (exit {enable}) — check the unit");
        }
        println!("veles gateway: installed + started (systemd user unit)");
        println!("  status:  systemctl --user status {NAME}");
        println!("  logs:    journalctl --user -u {NAME} -f");
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<()> {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", NAME])
            .status();
        let unit = unit_path();
        if unit.exists() {
            std::fs::remove_file(&unit)?;
            println!("removed {unit:?}");
        }
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("veles gateway: uninstalled");
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::NAME;
    use crate::veles_core;

    fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
        std::process::Command::new("schtasks").args(args).output()
    }

    fn query() -> Option<String> {
        let o = schtasks(&["/Query", "/TN", NAME, "/FO", "LIST"]).ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    pub fn installed() -> bool {
        query().is_some()
    }

    pub fn status() -> (String, bool) {
        match query() {
            None => (
                "  task:    not installed (`veles gateway install` to add a logon task)".into(),
                false,
            ),
            Some(text) => {
                let state = text
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("Status:"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "?".into());
                let active = state.eq_ignore_ascii_case("running");
                (format!("  task:    {NAME} -> {state}"), active)
            }
        }
    }

    /// The task's process is the launcher, gone at once; the gateway is
    /// its child. So a stop is the pid file's business (the IPC
    /// `shutdown`, then the kill), which the caller does after this.
    pub fn stop() -> anyhow::Result<()> {
        let _ = schtasks(&["/End", "/TN", NAME]);
        super::super::gateway_stop_by_pid()
    }

    pub fn restart() -> anyhow::Result<()> {
        stop()?;
        let o = schtasks(&["/Run", "/TN", NAME])?;
        if !o.status.success() {
            anyhow::bail!(
                "schtasks /Run failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        println!("veles gateway: restarted (task {NAME})");
        Ok(())
    }

    /// The characters XML text and attribute values cannot carry raw.
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    /// The task definition `schtasks /Create /XML` registers. The
    /// command-line form (`/SC ONLOGON /RL LIMITED /IT`) is refused with
    /// "Access is denied" from a normal (non-elevated) shell: a logon
    /// trigger for *any* user needs an administrator, and schtasks has
    /// no switch for "this user's logon" — the XML form has
    /// (`LogonTrigger/UserId`), and a user may register a task that
    /// runs as themself from a plain shell (measured on winbox
    /// 2026-10-13: elevated ssh OK, a medium-integrity token denied,
    /// the XML form OK under the same token). The XML also sets what
    /// the command-line form defaults badly: no execution time limit
    /// (the default stops a task after 72 h — the gateway is not a job)
    /// and no battery rules (a laptop unplugged would lose its
    /// gateway). The launcher (crates/veles-launcher) supervises the
    /// gateway — systemd's `Restart=on-failure` + `StartLimitBurst` —
    /// so the task runs as long as the gateway does; `RestartOnFailure`
    /// here covers only a launch that failed (measured 2026-10-13: it
    /// does not restart an on-demand run whose action exited non-zero).
    fn task_xml(user: &str, launcher: &str, log: &str) -> String {
        let (user, launcher, log) = (xml_escape(user), xml_escape(launcher), xml_escape(log));
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Veles Agent Gateway (veles gateway install)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>5</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{launcher}</Command>
      <Arguments>"{log}"</Arguments>
    </Exec>
  </Actions>
</Task>
"#
        )
    }

    /// schtasks reads a task file as UTF-16 (it loads the text into a
    /// wide string first, so a file declared UTF-8 fails with "unable
    /// to switch the encoding"): little-endian with a byte-order mark,
    /// the form Task Scheduler itself exports.
    fn write_utf16(path: &std::path::Path, text: &str) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(2 + text.len() * 2);
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(path, bytes)
    }

    /// A logon task, interactive, limited privileges: the gateway runs
    /// in the desktop session (toasts, Tk, a headed browser, cua-driver
    /// all need that) whenever this user is logged on. The program is
    /// `veles-gw.exe` beside this binary, with the log path as its one
    /// argument. The definition is kept at `<data>\gateway-task.xml`
    /// for `schtasks /Query` to be compared against.
    pub fn install() -> anyhow::Result<()> {
        let exe = std::env::current_exe()?;
        let launcher = exe.with_file_name("veles-gw.exe");
        if !launcher.exists() {
            anyhow::bail!(
                "no {} beside {}: build veles-launcher and install both",
                launcher.display(),
                exe.display()
            );
        }
        let home = veles_core::config::veles_home();
        let log = home.join("gateway.log");
        let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(d), Ok(u)) if !d.is_empty() && !u.is_empty() => format!("{d}\\{u}"),
            (_, Ok(u)) if !u.is_empty() => u,
            _ => anyhow::bail!("USERNAME is not set: cannot name the task's user"),
        };
        let xml = task_xml(
            &user,
            &launcher.display().to_string(),
            &log.display().to_string(),
        );
        std::fs::create_dir_all(&home)?;
        let def = home.join("gateway-task.xml");
        write_utf16(&def, &xml)?;
        let def_s = def.display().to_string();
        let o = schtasks(&["/Create", "/TN", NAME, "/XML", &def_s, "/F"])?;
        if !o.status.success() {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            // A task another token registered (an elevated shell's, say)
            // cannot be replaced from this one: /F is "Access is denied"
            // although a fresh /Create would succeed (winbox,
            // 2026-10-13: the dev install's task, made over an elevated
            // ssh, blocked the installer running as the user's own
            // token). Name the way out instead of leaving the generic
            // error.
            if query().is_some() {
                anyhow::bail!(
                    "schtasks /Create failed: {err}\n  a task {NAME} already exists and \
                     this shell may not replace it (one registered from an elevated \
                     shell can only be replaced from one): run `schtasks /Delete /TN \
                     {NAME} /F` there, then `veles gateway install` again"
                );
            }
            anyhow::bail!("schtasks /Create failed: {err}");
        }
        println!(
            "registered task {NAME} (logon of {user}): \"{}\" \"{}\"",
            launcher.display(),
            log.display()
        );
        let o = schtasks(&["/Run", "/TN", NAME])?;
        if !o.status.success() {
            anyhow::bail!(
                "schtasks /Run failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        println!("veles gateway: installed + started (logon task)");
        println!("  status:  veles gateway status   (or schtasks /Query /TN {NAME})");
        println!("  logs:    {}", log.display());
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<()> {
        let _ = stop();
        let o = schtasks(&["/Delete", "/TN", NAME, "/F"])?;
        if !o.status.success() {
            anyhow::bail!(
                "schtasks /Delete failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        println!("veles gateway: uninstalled (task {NAME} removed)");
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn installed() -> bool {
        false
    }
    pub fn status() -> (String, bool) {
        (
            "  service: no service manager on this platform".into(),
            false,
        )
    }
    pub fn stop() -> anyhow::Result<()> {
        anyhow::bail!("no service manager on this platform")
    }
    pub fn restart() -> anyhow::Result<()> {
        anyhow::bail!("no service manager on this platform")
    }
    pub fn install() -> anyhow::Result<()> {
        anyhow::bail!("no service manager on this platform")
    }
    pub fn uninstall() -> anyhow::Result<()> {
        anyhow::bail!("no service manager on this platform")
    }
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}
