//! The phone's userland, seen from the engine (PLAN-phone-userland.md
//! T3 design items 4 and 5).
//!
//! The userland is a second app, `chat.janitor.veles.userland`: Alpine
//! under proot, run by an exec broker (crates/veles-userland). The engine
//! reaches it only through `ulsh`, the client the gateway APK ships as
//! `lib/arm64-v8a/libulsh.so`, and Userland.java points `VELES_SHELL` at
//! ulsh only after a probe command ran there at this engine's start. So,
//! to the engine, "the userland answers" is `VELES_SHELL` naming ulsh
//! ([`ulsh`]), and costs a stat.
//!
//! Two things are built on that here. The engine's OWN calls into the
//! distro — a user-env box's commands (`user_envs`, `[sandbox] backend =
//! "userland"`), the Tcl fence (`sandbox::UserlandBackend`), the caps
//! probe (`node`) — go through [`command`], which hands ulsh a cleared
//! environment: ulsh passes what it is given into the guest, and a box
//! must get none of the gateway's secrets (the rule `podman exec` keeps
//! by taking only `-e` pairs). And the platform facts (`platform`) say
//! what the distro changes about the phone.
//!
//! This crate does not depend on veles-userland (that one is unix-only;
//! veles-core builds for Windows too): the variable names below are its
//! contract, pinned by both sides' tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// ulsh's file names: the gateway APK's, and a host build's.
const ULSH_NAMES: &[&str] = &["libulsh.so", "ulsh"];

/// The variables that carry ulsh to the broker (the gateway's relay
/// socket, a direct socket, the broker's uid to hold it to): copied from
/// the engine's environment into every ulsh the engine starts on its own
/// account. Nothing else of the engine's environment is.
pub const TRANSPORT_ENV: &[&str] = &["ULSH_RELAY", "ULSH_SOCKET", "ULSH_BROKER_UID"];

/// The guest variable the broker sets to the sandbox's name and strips
/// from every other session (veles-userland `proto::SANDBOX_MARKER`).
pub const SANDBOX_MARKER: &str = "VELES_SANDBOX";

/// Whether `shell` (a `VELES_SHELL` value) is ulsh, by its file name.
pub fn is_ulsh(shell: &str) -> bool {
    Path::new(shell.trim())
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| ULSH_NAMES.contains(&n))
}

/// ulsh, when it is this engine's shell and the file is there: the
/// userland answered at this engine's start (module doc).
pub fn ulsh() -> Option<PathBuf> {
    let s = std::env::var(crate::shell::SHELL_ENV).ok()?;
    let s = s.trim();
    (is_ulsh(s) && Path::new(s).is_file()).then(|| PathBuf::from(s))
}

/// Whether the userland answers (see [`ulsh`]).
pub fn in_use() -> bool {
    ulsh().is_some()
}

/// One program the engine runs in the userland on its own account.
#[derive(Debug, Clone)]
pub struct GuestCall<'a> {
    /// The named sandbox (a guest root of its own under the userland's
    /// `files/sandboxes`), or `None` for the rootfs itself.
    pub sandbox: Option<&'a str>,
    /// The guest's working directory.
    pub cwd: &'a str,
    /// The guest's HOME; `None` is ulsh's own, `/root`.
    pub home: Option<&'a str>,
    /// The guest's variables beyond ulsh's basics. ulsh sets PATH,
    /// HOME, TMPDIR, SHELL, TERM and LANG itself (HOME through `home`),
    /// so a HOME or TMPDIR here does not reach the guest.
    pub env: &'a [(&'a str, String)],
    /// The program and its arguments inside the guest, run with no shell
    /// in between (`ulsh --exec`).
    pub argv: &'a [String],
}

/// `ulsh --exec ARGV` for `call`: the environment cleared, then given the
/// transport ([`TRANSPORT_ENV`], read through `ambient`), the call's
/// place (`ULSH_SANDBOX`, `ULSH_CWD`, `ULSH_HOME`) and its own variables
/// — nothing else of the engine's. stdio is the caller's to set.
pub fn command(
    ulsh: &Path,
    call: &GuestCall,
    ambient: impl Fn(&str) -> Option<String>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(ulsh);
    cmd.env_clear();
    for var in TRANSPORT_ENV {
        if let Some(v) = ambient(var).filter(|v| !v.trim().is_empty()) {
            cmd.env(var, v);
        }
    }
    if let Some(name) = call.sandbox {
        cmd.env("ULSH_SANDBOX", name);
    }
    cmd.env("ULSH_CWD", call.cwd);
    if let Some(home) = call.home {
        cmd.env("ULSH_HOME", home);
    }
    for (k, v) in call.env {
        // ulsh's own knobs are this function's to set, and the marker is
        // the broker's word: neither may come in through a call's env.
        if k.starts_with("ULSH_") || *k == SANDBOX_MARKER {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.arg("--exec").args(call.argv);
    cmd
}

/// The shell text that prints which of `names` the guest has, one per
/// line. Names are program names (no quoting needed; anything else is
/// left out rather than spliced into a script).
pub fn guest_probe_script(names: &[&str]) -> String {
    let words: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| {
            !n.is_empty()
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
        })
        .collect();
    format!(
        "for p in {}; do command -v \"$p\" >/dev/null 2>&1 && echo \"$p\"; done; exit 0",
        words.join(" ")
    )
}

/// The probe's output read back: the names it printed that were asked
/// about, in the order asked, each once. Anything else (a motd, a
/// warning) is not a program.
pub fn parse_guest_probe(out: &str, names: &[&str]) -> Vec<String> {
    let said: std::collections::HashSet<&str> = out.lines().map(str::trim).collect();
    names
        .iter()
        .filter(|n| said.contains(*n))
        .map(|n| n.to_string())
        .collect()
}

/// Which of `names` the userland's rootfs has: one ulsh session running
/// [`guest_probe_script`], bounded by `timeout` (killed and reaped past
/// it, like the GPU probe). Empty when the session fails.
pub fn probe_guest(ulsh: &Path, names: &[&str], timeout: Duration) -> Vec<String> {
    use std::io::Read;
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        guest_probe_script(names),
    ];
    let call = GuestCall {
        sandbox: None,
        cwd: "/",
        home: None,
        env: &[],
        argv: &argv,
    };
    let mut cmd = command(ulsh, &call, |k| std::env::var(k).ok());
    let Ok(mut child) = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    "node: the userland did not answer its tool probe within {timeout:?} — \
                     advertising the APK's own tools only"
                );
                return Vec::new();
            }
        }
    }
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut text);
    }
    parse_guest_probe(&text, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(c: &std::process::Command) -> Vec<(String, Option<String>)> {
        c.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn val(c: &std::process::Command, k: &str) -> Option<String> {
        env_of(c)
            .into_iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| v)
    }

    fn args_of(c: &std::process::Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn ulsh_is_known_by_its_file_name() {
        assert!(is_ulsh(
            "/data/app/~~x==/chat.janitor.veles-y==/lib/arm64/libulsh.so"
        ));
        assert!(is_ulsh("ulsh"));
        assert!(is_ulsh(" /usr/local/bin/ulsh "));
        for not in [
            "/data/user/0/chat.janitor.veles/files/bin/bash",
            "sh",
            "",
            "libulsh.so.bak",
        ] {
            assert!(!is_ulsh(not), "{not:?}");
        }
    }

    /// The engine's own calls hand ulsh the transport and the call's
    /// place, and NOTHING else of the engine's environment: ulsh passes
    /// its environment into the guest, so a secret left in it would land
    /// in every box.
    #[test]
    fn an_engine_call_carries_the_transport_and_nothing_else() {
        let ambient = |k: &str| match k {
            "ULSH_RELAY" => Some("veles-userland-local".to_string()),
            "ULSH_BROKER_UID" => Some("10413".to_string()),
            "ULSH_SOCKET" => Some("  ".to_string()),
            "OPENAI_API_KEY" | "DESKTOP_BEARER_TOKEN" => Some("secret".to_string()),
            _ => None,
        };
        let argv = ["rg".to_string(), "-n".to_string(), "x".to_string()];
        let env = [
            (
                "VELES_SANDBOX_SCRIPT",
                "/work/.veles-run/1/user.tcl".to_string(),
            ),
            ("ULSH_SANDBOX", "someone-elses-box".to_string()),
            (SANDBOX_MARKER, "forged".to_string()),
        ];
        let call = GuestCall {
            sandbox: Some("dm_local_owner-1a2b3c4d"),
            cwd: "/work",
            home: Some("/work"),
            env: &env,
            argv: &argv,
        };
        let c = command(Path::new("/lib/libulsh.so"), &call, ambient);
        assert_eq!(c.get_program(), "/lib/libulsh.so");
        assert_eq!(args_of(&c), ["--exec", "rg", "-n", "x"]);
        let mut names: Vec<String> = env_of(&c)
            .into_iter()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k)
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "ULSH_BROKER_UID",
                "ULSH_CWD",
                "ULSH_HOME",
                "ULSH_RELAY",
                "ULSH_SANDBOX",
                "VELES_SANDBOX_SCRIPT"
            ],
            "only the transport, the place and the call's own"
        );
        assert_eq!(
            val(&c, "ULSH_SANDBOX").as_deref(),
            Some("dm_local_owner-1a2b3c4d"),
            "the call's box, not one smuggled in through its env"
        );
        assert_eq!(val(&c, "ULSH_CWD").as_deref(), Some("/work"));
        assert_eq!(val(&c, "ULSH_HOME").as_deref(), Some("/work"));
        // env_clear: the engine's environment is not inherited.
        let std_env = format!("{c:?}");
        assert!(!std_env.contains("secret"), "{std_env}");
    }

    /// The same, measured: a fake ulsh that prints its environment sees
    /// the transport and the call, and not one variable of this process's
    /// own (which `get_envs` above cannot show: it lists what was SET).
    #[cfg(unix)]
    #[test]
    fn the_engines_own_environment_never_reaches_ulsh() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("ulsh");
        // `export -p`, a builtin: the cleared environment has no PATH to
        // find an `env` on (and a nix build sandbox has no /usr/bin/env).
        std::fs::write(&fake, "#!/bin/sh\nexport -p\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let argv = ["/bin/true".to_string()];
        let call = GuestCall {
            sandbox: Some("box-1"),
            cwd: "/work",
            home: Some("/work"),
            env: &[],
            argv: &argv,
        };
        let out = command(&fake, &call, |k| {
            (k == "ULSH_RELAY").then(|| "veles-userland-local".to_string())
        })
        .output()
        .unwrap();
        let env = String::from_utf8_lossy(&out.stdout);
        // `export NAME='v'` (dash) or `declare -x NAME="v"` (bash).
        let names: Vec<&str> = env
            .lines()
            .filter_map(|l| {
                let l = l
                    .strip_prefix("declare -x ")
                    .or(l.strip_prefix("export "))?;
                Some(l.split('=').next().unwrap_or(l))
            })
            .collect();
        assert!(names.contains(&"ULSH_RELAY"), "{env}");
        assert!(names.contains(&"ULSH_SANDBOX"), "{env}");
        assert!(
            env.contains("veles-userland-local") && env.contains("box-1"),
            "{env}"
        );
        for leaked in ["HOME", "USER", "RUST_MIN_STACK", "CARGO_PKG_NAME"] {
            assert!(
                !names.contains(&leaked),
                "{leaked} reached ulsh from the engine's environment: {env}"
            );
        }
    }

    #[test]
    fn a_rootfs_call_names_no_sandbox() {
        let argv = ["/bin/true".to_string()];
        let call = GuestCall {
            sandbox: None,
            cwd: "/",
            home: None,
            env: &[],
            argv: &argv,
        };
        let c = command(Path::new("ulsh"), &call, |_| None);
        assert_eq!(val(&c, "ULSH_SANDBOX"), None);
        assert_eq!(val(&c, "ULSH_HOME"), None);
        assert_eq!(val(&c, "ULSH_CWD").as_deref(), Some("/"));
    }

    #[test]
    fn the_guest_probe_reads_back_only_what_it_asked() {
        let names = ["bash", "apk", "git", "rg"];
        let script = guest_probe_script(&["bash", "apk", "a;rm -rf /", "git"]);
        assert!(script.contains("for p in bash apk git;"), "{script}");
        assert!(!script.contains("rm -rf"), "{script}");
        let got = parse_guest_probe("welcome to Alpine!\ngit\napk\nbash\napk\nnode\n", &names);
        assert_eq!(got, ["bash", "apk", "git"], "asked order, once each");
        assert!(parse_guest_probe("", &names).is_empty());
    }
}
