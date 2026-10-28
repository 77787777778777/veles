//! Shell resolution for `-c` command execution.
//!
//! Veles spawns `bash -c <cmd>` in several surfaces (the terminal
//! tool, background shells, monitors, `.md` command execution, TUI
//! `/diff`, the chat surface git helpers). Bash was hardcoded, which
//! assumes bash is on PATH — true here, not guaranteed on every
//! deployment target. This module centralizes the choice so the
//! fallback and override live in ONE place.
//!
//! Resolution order:
//! 1. `VELES_SHELL` env (explicit operator override — escape hatch).
//! 2. The platform's default (`platform::default_shell`): `bash` if it
//!    is on PATH, else `sh` (POSIX `sh -c` covers what veles emits);
//!    on Windows `pwsh`, else `powershell`, else `cmd`.
//!
//! The binary AND its "run this script" arguments are resolved here
//! ([`shell_args`], [`command`]): `-c` is POSIX, PowerShell wants
//! `-NoProfile -NonInteractive -Command`, cmd wants `/D /C`. The call
//! sites keep their own stdio/kill_on_drop posture.

use std::sync::OnceLock;

/// Operator override: force a specific shell binary (absolute path or
/// PATH name). Empty/unset falls through to discovery.
pub const SHELL_ENV: &str = "VELES_SHELL";

static RESOLVED: OnceLock<String> = OnceLock::new();

/// The shell binary to spawn for `-c` execution. Cached after first
/// resolution — PATH does not change mid-process, and the terminal tool
/// runs this on every invocation otherwise.
pub fn shell_binary() -> &'static str {
    RESOLVED.get_or_init(resolve).as_str()
}

/// The shell resolved NOW, uncached: what [`shell_binary`] caches, read
/// again. For a caller whose answer must follow `VELES_SHELL` as it is
/// at the call (the sandbox's memory wrapper, and its test).
pub fn resolve() -> String {
    if let Ok(explicit) = std::env::var(SHELL_ENV) {
        let explicit = explicit.trim().to_string();
        if !explicit.is_empty() {
            return explicit;
        }
    }
    // bash, else the POSIX floor; pwsh, else Windows PowerShell, else
    // cmd (USER 2026-10-13) — the platform layer knows which.
    crate::platform::default_shell()
}

/// The arguments that make [`shell_binary`] run `script`: `-c` on a
/// POSIX shell, `-NoProfile -NonInteractive -Command` on PowerShell,
/// `/D /C` on cmd. Every spawn of the shell goes through this, so the
/// platform's shell dialect is decided in one place.
pub fn shell_args(script: &str) -> Vec<String> {
    crate::platform::shell_args(shell_binary(), script)
}

/// A `Command` for `<shell> <args that run script>`, stdio untouched.
pub fn command(script: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(shell_binary());
    c.args(shell_args(script));
    c
}

/// [`command`] as a std `Command`.
pub fn std_command(script: &str) -> std::process::Command {
    let mut c = std::process::Command::new(shell_binary());
    c.args(shell_args(script));
    c
}

/// Resolve a command name against PATH, the way a shell would.
///
/// An absolute or explicitly-relative path is returned as-is when it
/// exists — an operator who set `VELES_PODMAN_BIN=/opt/bin/podman`
/// means that file, not a PATH lookup for a name containing slashes.
///
/// `doctor --sandbox` reports the resolved path rather than a bare
/// yes/no: on a machine with three podmans, which one the fence will
/// actually use is the answer worth printing.
pub fn which(name: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(name);
    if p.components().count() > 1 || p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    // On Windows a bare `podman` is `podman.exe`, and an npm shim is
    // `agent-browser.cmd`: PATHEXT says which spellings to try.
    let candidates = crate::platform::exe_candidates(name);
    std::env::split_paths(&path)
        .flat_map(|dir| candidates.iter().map(move |c| dir.join(c)))
        .find(|c| c.is_file())
}

/// The program to hand `Command::new` for a configured command name:
/// the resolved path when PATH has it under a platform spelling
/// (`.exe`, `.cmd`), else the name as given, so a shell builtin or a
/// relative path still reaches the OS unchanged.
pub fn resolve_program(name: &str) -> std::path::PathBuf {
    which(name).unwrap_or_else(|| std::path::PathBuf::from(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_to_a_real_shell() {
        // Whatever it picks must exist on this machine and accept -c.
        let bin = shell_binary();
        assert!(!bin.is_empty());
        let probe = std::process::Command::new(bin)
            .arg("-c")
            .arg("echo veles-shell-probe")
            .output();
        match probe {
            Ok(o) => {
                assert!(
                    String::from_utf8_lossy(&o.stdout).contains("veles-shell-probe"),
                    "{bin} must run -c"
                );
            }
            Err(e) => panic!("resolved shell {bin} is not spawnable: {e}"),
        }
    }

    // ── RM3 #7: what models expect from a shell tool, pinned ─────────
    //
    // USER: "make sure shell scripts and terminal always run in bash
    // (or sh if bash is unavailable), since that is what most models
    // expect. But it has to inherit the environment from the parent
    // shell to account for things like direnv/envrc."

    /// Bash when it exists, `sh` only as the floor, `VELES_SHELL` as
    /// the operator override — and NEVER a non-shell direct exec.
    /// (Windows has its own ladder, pinned below.)
    #[cfg(unix)]
    #[test]
    fn bash_is_preferred_and_sh_is_the_floor() {
        // Another test points VELES_SHELL at a scratch shell under this
        // lock (sandbox.rs); reading the variable outside it raced that.
        let _g = crate::testenv::lock();
        // The dev/test environment has bash on PATH and no override
        // set; if an operator's override IS set, honour it instead.
        if let Ok(explicit) = std::env::var(SHELL_ENV) {
            if !explicit.trim().is_empty() {
                assert_eq!(shell_binary(), explicit.trim());
                return;
            }
        }
        assert_eq!(
            shell_binary(),
            "bash",
            "bash is on PATH in this environment; the floor (sh) must not win"
        );
    }

    /// The Windows ladder (PLAN-windows.md §1): pwsh, then Windows
    /// PowerShell, then cmd — and pwsh when it is there, as it is on
    /// every host this ran on.
    #[cfg(windows)]
    #[test]
    fn pwsh_is_preferred_and_cmd_is_the_floor() {
        let _g = crate::testenv::lock();
        if let Ok(explicit) = std::env::var(SHELL_ENV) {
            if !explicit.trim().is_empty() {
                assert_eq!(shell_binary(), explicit.trim());
                return;
            }
        }
        let got = shell_binary();
        assert!(
            ["pwsh", "powershell", "cmd"].contains(&got),
            "a Windows shell, never a direct exec: {got}"
        );
        if which("pwsh").is_some() {
            assert_eq!(got, "pwsh", "pwsh is on PATH; the floor must not win");
        }
    }

    /// The environment of the gateway process reaches the child:
    /// direnv exports ride along because nothing scrubs env.
    #[test]
    fn child_processes_inherit_the_parent_environment() {
        // Unique name; set before spawning, read back by the child.
        std::env::set_var("VELES_SHELL_PIN", "inherited-42");
        let script = if cfg!(windows) {
            "Write-Host -NoNewline $env:VELES_SHELL_PIN"
        } else {
            "printf '%s' \"$VELES_SHELL_PIN\""
        };
        let out = std_command(script).output().expect("spawn");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "inherited-42");
    }

    /// And it really is a SHELL: pipes and globs — the constructs
    /// models put in `command` on the assumption a shell parses them.
    #[test]
    fn commands_run_under_shell_semantics() {
        let script = if cfg!(windows) {
            "'veles-pin-marker' | ForEach-Object { $_.ToUpper() }"
        } else {
            "echo veles-pin-marker | tr a-z A-Z"
        };
        let out = std_command(script).output().expect("spawn");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("VELES-PIN-MARKER"),
            "a pipe must be interpreted by a shell, not exec'd literally"
        );
    }
}
