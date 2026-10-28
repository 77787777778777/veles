//! The client half: what `ulsh` does, as a library the tests drive.
//!
//! [`start`] connects, checks the broker is who it should be (when told
//! whom to expect), sends the request with the stdio descriptors, and
//! returns a [`Session`] once the broker says the program started.
//! [`Session::wait`] is the program's exit status. A [`Controller`]
//! (any thread) forwards signals and pty sizes meanwhile.
//!
//! On the phone the broker's socket cannot be connected to from the
//! gateway's app (SELinux, PLAN-phone-userland.md T3 §2 R1b): ulsh asks
//! the gateway's own relay for a session socket instead ([`open_relay`],
//! `ULSH_RELAY`) and starts on it ([`start_on`]); `proto` draws the hops.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use crate::proto::{self, Control, Relay, Reply, Request};
use crate::sock;

/// The guest's PATH: Alpine's, as its /etc/profile has it.
pub const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// `sh -c WRAP arg0 script args…`: the script under bash when the rootfs
/// has it (Alpine's minirootfs does not until `apk add bash`), else
/// under the rootfs's sh. ulsh cannot look inside the rootfs, so the
/// rootfs decides.
pub const WRAP: &str = r#"s=$1; shift; [ -x /bin/bash ] && exec /bin/bash -c "$s" "$0" "$@"; exec /bin/sh -c "$s" "$0" "$@""#;

/// The same for an interactive login shell.
pub const WRAP_LOGIN: &str = r#"[ -x /bin/bash ] && exec /bin/bash -l; exec /bin/sh -l"#;

/// What ulsh was asked to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// argv inside the rootfs.
    pub argv: Vec<String>,
    /// `Some(true)` with -t; `None` = decide (a terminal on stdin).
    pub pty: Option<bool>,
}

/// ulsh's command line, the subset of a shell's the gateway uses:
/// `-c SCRIPT [ARG0 [ARGS…]]` (also spelled `-lc`, `-ic`), nothing for a
/// login shell, `-t` first for a pty, `--exec PROG ARGS…` for a program
/// without a shell.
pub fn parse_args(args: &[String]) -> Result<Invocation, String> {
    let mut pty = None;
    let mut args = args;
    if args.first().map(String::as_str) == Some("-t") {
        pty = Some(true);
        args = &args[1..];
    }
    let Some(first) = args.first() else {
        return Ok(Invocation {
            argv: vec!["/bin/sh".into(), "-c".into(), WRAP_LOGIN.into()],
            pty,
        });
    };
    if first == "--exec" {
        if args.len() < 2 {
            return Err("--exec needs a program".into());
        }
        return Ok(Invocation {
            argv: args[1..].to_vec(),
            pty,
        });
    }
    let is_c = first.len() >= 2
        && first.starts_with('-')
        && !first.starts_with("--")
        && first[1..].chars().all(|c| c.is_ascii_alphabetic())
        && first.contains('c');
    if is_c {
        let Some(script) = args.get(1) else {
            return Err(format!("{first} needs a script"));
        };
        let arg0 = args.get(2).cloned().unwrap_or_else(|| "ulsh".into());
        let mut argv = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            WRAP.into(),
            arg0,
            script.clone(),
        ];
        argv.extend(args.iter().skip(3).cloned());
        return Ok(Invocation { argv, pty });
    }
    Err(format!(
        "usage: ulsh [-t] [-c SCRIPT [ARG0 ARGS…] | --exec PROG ARGS…] (got {first:?})"
    ))
}

/// Host variables that mean nothing inside the rootfs, or mean the wrong
/// thing there: the gateway's paths (its native library directory, its
/// app storage, Android's runtime) and settings for the gateway's own
/// static git and Tcl.
const DROP: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "TMPDIR",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "BOOTCLASSPATH",
    "DEX2OATBOOTCLASSPATH",
    "SYSTEMSERVERCLASSPATH",
    "STANDALONE_SYSTEMSERVER_JARS",
    "EXTERNAL_STORAGE",
    "ASEC_MOUNTPOINT",
    "TCL_LIBRARY",
    "GIT_EXEC_PATH",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_COUNT",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "VELES_SHELL",
    "VELES_RG",
    "VELES_TCLSH",
    "VELES_MODULE_PATH",
    "VELES_LOCALES_DIR",
];
const DROP_PREFIX: &[&str] = &[
    "ANDROID_",
    "GIT_CONFIG_KEY_",
    "GIT_CONFIG_VALUE_",
    "PROOT_",
    "ULSH_",
];

/// The guest's environment from the host's: [`DROP`] dropped, the FHS
/// basics set. Everything else passes, the secrets the gateway injected
/// for this command among them. HOME is `/root`, or `ULSH_HOME` when that
/// is a guest path (veles' user envs: `/work`, as their podman boxes have
/// it).
pub fn guest_env(
    host: impl Iterator<Item = (String, String)>,
    pty: bool,
) -> BTreeMap<String, String> {
    let mut home = "/root".to_string();
    let mut env: BTreeMap<String, String> = host
        .inspect(|(k, v)| {
            if k == "ULSH_HOME" && v.starts_with('/') {
                home = v.clone();
            }
        })
        .filter(|(k, _)| !DROP.contains(&k.as_str()))
        .filter(|(k, _)| !DROP_PREFIX.iter().any(|p| k.starts_with(p)))
        .collect();
    env.insert("PATH".into(), GUEST_PATH.into());
    env.insert("HOME".into(), home);
    env.insert("TMPDIR".into(), "/tmp".into());
    env.insert("SHELL".into(), "/bin/sh".into());
    env.entry("LANG".into()).or_insert_with(|| "C.UTF-8".into());
    let term = if pty { "xterm-256color" } else { "dumb" };
    env.entry("TERM".into()).or_insert_with(|| term.into());
    env
}

/// The sandbox `ULSH_SANDBOX` names: `None` when unset or blank, an
/// error (said before any session is opened) when it is not a name.
pub fn sandbox_from(var: Option<String>) -> Result<Option<String>, String> {
    let Some(v) = var.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if proto::valid_sandbox_name(&v) {
        Ok(Some(v))
    } else {
        Err(format!(
            "ULSH_SANDBOX={v:?} is not a sandbox name (letters, digits, `_`, `-` and `.`, \
             not starting with `.` or `-`, up to {} bytes)",
            proto::MAX_SANDBOX_NAME
        ))
    }
}

/// Sends controls to the broker from any thread.
#[derive(Clone)]
pub struct Controller(Arc<Mutex<UnixStream>>);

impl Controller {
    pub fn signal(&self, sig: i32) -> io::Result<()> {
        let s = self.0.lock().unwrap();
        proto::send(s.as_raw_fd(), &Control::Signal(sig), &[])
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let s = self.0.lock().unwrap();
        proto::send(s.as_raw_fd(), &Control::Resize { rows, cols }, &[])
    }
}

/// A started program.
pub struct Session {
    conn: UnixStream,
    ctl: Controller,
    /// Its pid, in the userland app (for logs).
    pub pid: i32,
    /// The pty master, for a pty request.
    pub master: Option<OwnedFd>,
}

impl Session {
    pub fn controller(&self) -> Controller {
        self.ctl.clone()
    }

    /// Waits for the program's status: its exit code, or 128 + signal.
    pub fn wait(&self) -> io::Result<i32> {
        loop {
            match proto::recv::<Reply>(self.conn.as_raw_fd(), 0)? {
                Some((Reply::Exit(code), _)) => return Ok(code),
                Some((Reply::Error(m), _)) => return Err(io::Error::other(m)),
                Some((Reply::Started { .. }, _)) => continue,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the userland broker hung up before the program ended",
                    ))
                }
            }
        }
    }
}

/// Connects to `@socket` and starts `req`. `broker_uid`: refuse a
/// broker running as anyone else (`ULSH_BROKER_UID`, which the gateway
/// looks up for the userland package). `stdio`: the program's stdin,
/// stdout, stderr (ignored for a pty request).
pub fn start(
    socket: &str,
    broker_uid: Option<u32>,
    req: &Request,
    stdio: [RawFd; 3],
) -> io::Result<Session> {
    let conn = sock::connect(socket).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot reach the userland broker on @{socket}: {e} (is chat.janitor.veles.userland installed and started?)"),
        )
    })?;
    start_on(conn, broker_uid, req, stdio)
}

/// [`start`] on a stream that already reaches the broker: a session
/// socket from [`open_relay`] (on the phone, one the userland service
/// made and handed over Binder), or any connected one. `broker_uid`
/// holds its peer — for a socketpair, whoever made the pair — to the
/// userland package's uid.
pub fn start_on(
    conn: UnixStream,
    broker_uid: Option<u32>,
    req: &Request,
    stdio: [RawFd; 3],
) -> io::Result<Session> {
    if let Some(want) = broker_uid {
        let got = sock::peer_uid(&conn)?;
        if got != want {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the broker's socket is served by uid {got}, not the userland package's uid {want}: not handing it anything"),
            ));
        }
    }
    let fd = conn.as_raw_fd();
    let fds: &[RawFd] = if req.pty { &[] } else { &stdio };
    let sent = proto::send(fd, req, fds);
    // A broker that refuses us has said why before hanging up, which can
    // make our send fail: read the reason either way.
    let reply = proto::recv::<Reply>(fd, 1);
    match reply {
        Ok(Some((Reply::Started { pid }, mut got))) => {
            sent?;
            let master = if req.pty {
                Some(got.pop().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "pty started, no master came")
                })?)
            } else {
                None
            };
            let ctl = Controller(Arc::new(Mutex::new(conn.try_clone()?)));
            Ok(Session {
                conn,
                ctl,
                pid,
                master,
            })
        }
        Ok(Some((Reply::Error(m), _))) => Err(io::Error::new(io::ErrorKind::PermissionDenied, m)),
        Ok(Some((Reply::Exit(_), _))) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker sent an exit before a start",
        )),
        Ok(None) => Err(sent.err().unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the userland broker hung up without answering",
            )
        })),
        Err(e) => Err(sent.err().unwrap_or(e)),
    }
}

/// A session socket from the gateway's relay on `@name`
/// (`ULSH_RELAY`; UserlandRelay.java serves it): the relay must be
/// `relay_uid` (the gateway app, ulsh's own uid), and answers
/// `Relay::Opened` with the socket, or why there is none.
pub fn open_relay(name: &str, relay_uid: u32) -> io::Result<UnixStream> {
    let conn = sock::connect(name).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot reach the gateway's userland relay on @{name}: {e}"),
        )
    })?;
    let got = sock::peer_uid(&conn)?;
    if got != relay_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("@{name} is served by uid {got}, not this app's uid {relay_uid}: not taking a session from it"),
        ));
    }
    match proto::recv::<Relay>(conn.as_raw_fd(), 1)? {
        Some((Relay::Opened, mut fds)) => fds.pop().map(UnixStream::from).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the relay said opened, but no session socket came with it",
            )
        }),
        Some((Relay::Refused(why), _)) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("no userland session: {why}"),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the gateway's userland relay hung up without a session",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn dash_c_runs_the_script_under_bash_else_sh() {
        let inv = parse_args(&s(&["-c", "echo hi"])).unwrap();
        assert_eq!(inv.argv, s(&["/bin/sh", "-c", WRAP, "ulsh", "echo hi"]));
        assert_eq!(inv.pty, None);
        let inv = parse_args(&s(&["-lc", "x", "name", "a", "b"])).unwrap();
        assert_eq!(inv.argv, s(&["/bin/sh", "-c", WRAP, "name", "x", "a", "b"]));
        let inv = parse_args(&s(&["-t", "--exec", "/usr/bin/top"])).unwrap();
        assert_eq!(inv.argv, s(&["/usr/bin/top"]));
        assert_eq!(inv.pty, Some(true));
        assert_eq!(parse_args(&[]).unwrap().argv[2], WRAP_LOGIN);
        assert!(parse_args(&s(&["-c"])).is_err());
        assert!(parse_args(&s(&["--frobnicate"])).is_err());
        assert!(parse_args(&s(&["script.sh"])).is_err());
    }

    #[test]
    fn the_guest_env_drops_the_gateways_paths_and_keeps_the_rest() {
        let host = [
            (
                "PATH",
                "/system/bin:/data/user/0/chat.janitor.veles/files/bin",
            ),
            ("LD_LIBRARY_PATH", "/data/app/x/lib/arm64"),
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "http.sslCAInfo"),
            ("ANDROID_ROOT", "/system"),
            ("ULSH_BROKER_UID", "10200"),
            ("OPENAI_API_KEY", "injected-for-this-command"),
            ("LANG", "en_GB.UTF-8"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let env = guest_env(host, false);
        assert_eq!(env["PATH"], GUEST_PATH);
        assert_eq!(env["HOME"], "/root");
        assert_eq!(env["TMPDIR"], "/tmp");
        assert_eq!(env["TERM"], "dumb");
        assert_eq!(env["LANG"], "en_GB.UTF-8");
        assert_eq!(env["OPENAI_API_KEY"], "injected-for-this-command");
        for gone in [
            "LD_LIBRARY_PATH",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "ANDROID_ROOT",
            "ULSH_BROKER_UID",
        ] {
            assert!(!env.contains_key(gone), "{gone} leaked into the guest");
        }
    }

    #[test]
    fn ulsh_home_is_the_guests_home_when_it_is_a_guest_path() {
        let with = |v: &str| {
            guest_env(
                [("ULSH_HOME".to_string(), v.to_string())].into_iter(),
                false,
            )
        };
        let env = with("/work");
        assert_eq!(env["HOME"], "/work");
        assert!(!env.contains_key("ULSH_HOME"));
        assert_eq!(with("work")["HOME"], "/root", "not a guest path");
        assert_eq!(guest_env(std::iter::empty(), false)["HOME"], "/root");
    }

    #[test]
    fn ulsh_sandbox_is_a_name_or_nothing() {
        assert_eq!(sandbox_from(None), Ok(None));
        assert_eq!(sandbox_from(Some("  ".into())), Ok(None));
        assert_eq!(sandbox_from(Some("box-1".into())), Ok(Some("box-1".into())));
        let err = sandbox_from(Some("../rootfs".into())).unwrap_err();
        assert!(err.contains("ULSH_SANDBOX"), "{err}");
    }
}
