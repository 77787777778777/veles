//! The broker and its client end to end on the host, with a fake proot
//! (a shell script that records its arguments, skips proot's options and
//! runs the program on the host in the `-w` directory). What the phone
//! adds — SELinux between two app domains, real proot — is the spike's
//! gate, PLAN-phone-userland.md T3.
//!
//! The uid checks are tested on a one-uid host by telling the broker
//! (or the client) to expect a uid that is not ours.
//!
//! The phone's transport (PLAN-phone-userland.md T3 §4): the two apps'
//! SELinux domains may not connect to each other's abstract sockets, so
//! a session reaches the broker as one end of a socketpair the userland
//! service made and handed over Binder — the broker gets the other end
//! on its handoff socket, ulsh gets its end from the gateway's own relay
//! socket. The tests play the userland service and the gateway's relay
//! in Rust ([`hand_over`], [`fake_relay`]); their Java
//! (UserlandService.java, UserlandRelay.java) writes the same frames.

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use veles_userland::proto::{self, Control, Reply, Request};
use veles_userland::{broker, client, sock};

const FAKE_PROOT: &str = r#"#!/bin/sh
# fake proot: record argv, skip proot's own options, run the rest here.
# The guest root it was given is FAKE_ROOT, for a script to act "inside".
printf '%s\n' "$@" > "$FAKE_PROOT_LOG"
cwd=/
while [ $# -gt 0 ]; do
  case $1 in
    -r) FAKE_ROOT=$2; export FAKE_ROOT; shift 2 ;;
    -b) shift 2 ;;
    -w) cwd=$2; shift 2 ;;
    -0|--*) shift ;;
    *) break ;;
  esac
done
cd "$cwd" || exit 99
exec "$@"
"#;

static N: AtomicU32 = AtomicU32::new(0);

struct Rig {
    dir: PathBuf,
    socket: String,
    /// The broker's handoff socket: the userland service's way in.
    handoff: String,
    cfg: broker::Config,
}

impl Rig {
    /// A broker on a fresh abstract name, serving `allow_uid`.
    fn new(allow_uid: u32) -> Rig {
        Rig::with_handoff(allow_uid, sock::my_uid())
    }

    /// The same, taking handed-over sessions from `handoff_uid` only
    /// (on the phone: the userland app's own uid).
    fn with_handoff(allow_uid: u32, handoff_uid: u32) -> Rig {
        Rig::build(allow_uid, handoff_uid, true)
    }

    /// A broker started without `--sandboxes`.
    fn without_sandboxes() -> Rig {
        Rig::build(sock::my_uid(), sock::my_uid(), false)
    }

    fn build(allow_uid: u32, handoff_uid: u32, sandboxes: bool) -> Rig {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("veles-userland-test-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tmp")).unwrap();
        std::fs::create_dir_all(dir.join("rootfs/etc")).unwrap();
        let proot = dir.join("proot");
        std::fs::write(&proot, FAKE_PROOT).unwrap();
        std::fs::set_permissions(&proot, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = format!("veles-userland-test-{pid}-{n}");
        let listener = sock::listen(&socket).unwrap();
        let handoff = format!("{socket}-handoff");
        let handoff_listener = sock::listen(&handoff).unwrap();
        let cfg = broker::Config {
            proot,
            loader: Some(dir.join("loader")),
            rootfs: dir.join("rootfs"),
            tmp: dir.join("tmp"),
            shared: Some(dir.join("shared")),
            allow_uid,
            handoff_uid,
            sandboxes: sandboxes.then(|| dir.join("sandboxes")),
        };
        let shared = Arc::new(cfg.clone());
        let h = shared.clone();
        std::thread::spawn(move || broker::serve(listener, shared));
        std::thread::spawn(move || broker::serve_handoff(handoff_listener, h));
        Rig {
            dir,
            socket,
            handoff,
            cfg,
        }
    }

    fn log(&self) -> PathBuf {
        self.dir.join("proot.log")
    }

    fn request(&self, script: &str, cwd: &Path) -> Request {
        let mut env = std::collections::BTreeMap::new();
        env.insert("FAKE_PROOT_LOG".into(), self.log().display().to_string());
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        Request {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env,
            cwd: cwd.display().to_string(),
            pty: false,
            winsize: None,
            sandbox: None,
        }
    }

    /// [`Rig::request`] in the sandbox `name`.
    fn in_sandbox(&self, name: &str, script: &str) -> Request {
        let mut r = self.request(script, &self.dir);
        r.sandbox = Some(name.into());
        r
    }

    fn sandbox(&self, name: &str) -> PathBuf {
        self.dir.join("sandboxes").join(name)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pipe() -> (std::fs::File, OwnedFd) {
    let mut p = [0; 2];
    assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    unsafe { (std::fs::File::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) }
}

fn devnull() -> std::fs::File {
    std::fs::File::open("/dev/null").unwrap()
}

/// Runs `req` with stdin from /dev/null; (status, stdout, stderr).
fn run(
    rig: &Rig,
    broker_uid: Option<u32>,
    req: &Request,
) -> std::io::Result<(i32, String, String)> {
    collect(|stdio| client::start(&rig.socket, broker_uid, req, stdio))
}

/// [`run`] on a stream that is already the broker's, as ulsh has it
/// when its session came over Binder and the gateway's relay.
fn run_on(
    conn: UnixStream,
    broker_uid: Option<u32>,
    req: &Request,
) -> std::io::Result<(i32, String, String)> {
    collect(|stdio| client::start_on(conn, broker_uid, req, stdio))
}

fn collect(
    start: impl FnOnce([RawFd; 3]) -> std::io::Result<client::Session>,
) -> std::io::Result<(i32, String, String)> {
    let (mut out_r, out_w) = pipe();
    let (mut err_r, err_w) = pipe();
    let null = devnull();
    let s = start([null.as_raw_fd(), out_w.as_raw_fd(), err_w.as_raw_fd()])?;
    drop((out_w, err_w, null));
    let code = s.wait()?;
    let (mut out, mut err) = (String::new(), String::new());
    out_r.read_to_string(&mut out).unwrap();
    err_r.read_to_string(&mut err).unwrap();
    Ok((code, out, err))
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_script_runs_through_proot_on_the_clients_own_stdio() {
    let rig = Rig::new(sock::my_uid());
    let req = rig.request("pwd; echo to-stderr >&2; exit 7", &rig.dir);
    let (code, out, err) = run(&rig, None, &req).unwrap();
    assert_eq!(code, 7, "the script's status comes back");
    assert_eq!(
        out.trim(),
        rig.dir.display().to_string(),
        "ran in -w's directory"
    );
    assert_eq!(err.trim(), "to-stderr");
    // proot was asked for the rootfs, the binds, the cwd, then the program.
    let argv = std::fs::read_to_string(rig.log()).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    let joined = argv.join(" ");
    assert!(
        joined.contains(&format!("-r {}", rig.dir.join("rootfs").display())),
        "{joined}"
    );
    assert!(joined.contains("-b /dev -b /proc -b /sys"), "{joined}");
    assert!(
        joined.contains(&format!("-b {}:/shared", rig.dir.join("shared").display())),
        "{joined}"
    );
    assert_eq!(
        &argv[argv.len() - 3..],
        &["/bin/sh", "-c", "pwd; echo to-stderr >&2; exit 7"]
    );
}

#[test]
fn ulsh_s_wrapper_falls_back_to_sh_and_keeps_the_script_whole() {
    let rig = Rig::new(sock::my_uid());
    let inv = client::parse_args(&[
        "-c".to_string(),
        r#"printf '%s|' "$0" "it's" "$#""#.to_string(),
    ])
    .unwrap();
    let mut req = rig.request("unused", &rig.dir);
    req.argv = inv.argv;
    let (code, out, _) = run(&rig, None, &req).unwrap();
    assert_eq!(code, 0);
    assert_eq!(out, "ulsh|it's|0|", "the script reached a shell intact");
}

#[test]
fn a_peer_whose_uid_is_not_the_gateways_is_refused_before_anything_runs() {
    let rig = Rig::new(sock::my_uid() + 1);
    let req = rig.request("echo ran", &rig.dir);
    let err = run(&rig, None, &req).expect_err("another uid must be refused");
    assert!(err.to_string().contains("may not use this broker"), "{err}");
    assert!(!rig.log().exists(), "proot must not have been started");
}

#[test]
fn ulsh_refuses_a_broker_that_is_not_the_userland_package() {
    let rig = Rig::new(sock::my_uid());
    let req = rig.request("echo ran", &rig.dir);
    let err = run(&rig, Some(sock::my_uid() + 1), &req).expect_err("wrong broker uid");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    assert!(err.to_string().contains("not handing it anything"), "{err}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!rig.log().exists(), "nothing was sent, so nothing ran");
    // The right uid goes through.
    assert_eq!(
        run(&rig, Some(sock::my_uid()), &req).unwrap().1.trim(),
        "ran"
    );
}

#[test]
fn a_forwarded_sigterm_reaches_the_script_and_its_status_says_so() {
    let rig = Rig::new(sock::my_uid());
    let pidfile = rig.dir.join("pid");
    let req = rig.request(
        &format!("echo $$ > {}; exec sleep 30", pidfile.display()),
        &rig.dir,
    );
    let null = devnull();
    let s = client::start(&rig.socket, None, &req, [null.as_raw_fd(); 3]).unwrap();
    wait_for("the script to start", || {
        std::fs::read_to_string(&pidfile).is_ok_and(|s| s.ends_with('\n'))
    });
    s.controller().signal(libc::SIGTERM).unwrap();
    assert_eq!(s.wait().unwrap(), 128 + libc::SIGTERM);
}

#[test]
fn hanging_up_kills_the_whole_process_group() {
    let rig = Rig::new(sock::my_uid());
    let pidfile = rig.dir.join("pid");
    // A background child too: the kill is the group's, not proot's alone.
    let req = rig.request(
        &format!("sleep 30 & echo $! > {}; wait", pidfile.display()),
        &rig.dir,
    );
    let null = devnull();
    let s = client::start(&rig.socket, None, &req, [null.as_raw_fd(); 3]).unwrap();
    wait_for("the script to start", || {
        std::fs::read_to_string(&pidfile).is_ok_and(|s| s.ends_with('\n'))
    });
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid));
    drop(s);
    wait_for("the background sleep to die", || !alive(pid));
}

#[test]
fn a_pty_request_gets_a_terminal_and_its_master() {
    let rig = Rig::new(sock::my_uid());
    let mut req = rig.request("[ -t 0 ] && [ -t 1 ] && echo on-a-tty; stty size", &rig.dir);
    req.pty = true;
    req.winsize = Some((33, 101));
    let s = client::start(&rig.socket, None, &req, [-1; 3]).unwrap();
    let master = s
        .master
        .as_ref()
        .expect("the master came back")
        .try_clone()
        .unwrap();
    let mut m = std::fs::File::from(master);
    let mut out = Vec::new();
    // Reading ends in EIO once the program's side is closed.
    let mut buf = [0u8; 4096];
    loop {
        match m.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("on-a-tty"), "{out:?}");
    assert!(out.contains("33 101"), "the size reached the pty: {out:?}");
    assert_eq!(s.wait().unwrap(), 0);
}

#[test]
fn a_probe_that_connects_and_hangs_up_costs_nothing() {
    let rig = Rig::new(sock::my_uid());
    drop(sock::connect(&rig.socket).unwrap());
    // The broker still serves the next one.
    let req = rig.request("echo after-probe", &rig.dir);
    assert_eq!(run(&rig, None, &req).unwrap().1.trim(), "after-probe");
}

#[test]
fn a_raw_request_without_its_descriptors_is_answered_with_an_error() {
    let rig = Rig::new(sock::my_uid());
    let conn = UnixStream::connect_addr(
        &<std::os::unix::net::SocketAddr as std::os::linux::net::SocketAddrExt>::from_abstract_name(
            rig.socket.as_bytes(),
        )
        .unwrap(),
    )
    .unwrap();
    let req = rig.request("echo ran", &rig.dir);
    proto::send(conn.as_raw_fd(), &req, &[]).unwrap();
    let (reply, _) = proto::recv::<Reply>(conn.as_raw_fd(), 0).unwrap().unwrap();
    assert!(
        matches!(reply, Reply::Error(ref m) if m.contains("descriptors")),
        "{reply:?}"
    );
    // And a control frame on a dead conversation is harmless.
    let _ = proto::send(conn.as_raw_fd(), &Control::Signal(libc::SIGINT), &[]);
    assert!(!rig.log().exists());
}

// ── The phone's transport: sessions over Binder (PLAN-phone-userland.md T3 §4) ──

/// What UserlandService does for each Binder `openSession`: a socketpair,
/// one end to the broker's handoff socket with the caller's uid in the
/// frame, the other end the caller's. The service's copy of the broker's
/// end is closed once it is on its way, as here.
fn hand_over(handoff: &str, caller_uid: u32) -> std::io::Result<UnixStream> {
    let (ours, theirs) = UnixStream::pair()?;
    let h = sock::connect(handoff)?;
    proto::send(
        h.as_raw_fd(),
        &proto::Handoff { caller_uid },
        &[theirs.as_raw_fd()],
    )?;
    Ok(ours)
}

/// The gateway's relay (UserlandRelay.java) on `@name`: every connection
/// gets a fresh session from `open`, riding on `Relay::Opened`, or the
/// reason there is none.
fn fake_relay(name: &str, open: impl Fn() -> Result<UnixStream, String> + Send + 'static) {
    let l = sock::listen(name).unwrap();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(c) = c else { return };
            match open() {
                Ok(s) => {
                    let _ = proto::send(c.as_raw_fd(), &proto::Relay::Opened, &[s.as_raw_fd()]);
                }
                Err(m) => {
                    let _ = proto::send(c.as_raw_fd(), &proto::Relay::Refused(m), &[]);
                }
            }
        }
    });
}

/// A relay that hands every caller a session vouched for as `caller_uid`.
fn relay_to(rig: &Rig, caller_uid: u32) -> String {
    let relay = format!("{}-local", rig.socket);
    let handoff = rig.handoff.clone();
    fake_relay(&relay, move || {
        hand_over(&handoff, caller_uid).map_err(|e| e.to_string())
    });
    relay
}

#[test]
fn a_session_on_a_socketpair_end_runs_with_no_listener_and_no_peer_check() {
    // The direct listener would refuse us (it serves another uid); a
    // session handed over is vouched for by whoever handed it.
    let rig = Rig::new(sock::my_uid() + 1);
    let (ours, theirs) = UnixStream::pair().unwrap();
    let cfg = rig.cfg.clone();
    std::thread::spawn(move || broker::session(theirs, &cfg));
    let req = rig.request("pwd; echo on-the-pair >&2; exit 5", &rig.dir);
    let (code, out, err) = run_on(ours, Some(sock::my_uid()), &req).unwrap();
    assert_eq!(code, 5);
    assert_eq!(out.trim(), rig.dir.display().to_string());
    assert_eq!(err.trim(), "on-the-pair");
}

#[test]
fn a_session_handed_over_on_the_handoff_socket_runs_for_the_gateway() {
    // The gateway is another app: its uid reaches the broker only as the
    // service's word for it, never as the session socket's peer.
    let gateway = sock::my_uid() + 1;
    let rig = Rig::new(gateway);
    let conn = hand_over(&rig.handoff, gateway).unwrap();
    let req = rig.request("echo handed-over; exit 6", &rig.dir);
    // The pair's maker is the userland app (here: us), which is what
    // ULSH_BROKER_UID pins.
    let (code, out, _) = run_on(conn, Some(sock::my_uid()), &req).unwrap();
    assert_eq!((code, out.trim()), (6, "handed-over"));
}

#[test]
fn a_handoff_vouching_for_a_uid_the_broker_does_not_serve_is_refused() {
    let rig = Rig::new(sock::my_uid() + 1);
    let conn = hand_over(&rig.handoff, sock::my_uid() + 2).unwrap();
    let req = rig.request("echo ran", &rig.dir);
    let err = run_on(conn, None, &req).expect_err("another caller must be refused");
    assert!(err.to_string().contains("may not use this broker"), "{err}");
    assert!(!rig.log().exists(), "proot must not have been started");
}

#[test]
fn a_handoff_from_anyone_but_the_userland_app_is_dropped_unread() {
    // Only the userland app's own uid may hand sessions over. The broker
    // closes the connection on sight of another uid, so our frame lands
    // either before that close (the session then goes unserved) or after
    // it (the send fails with EPIPE); both are the drop. The second
    // shape showed once, under a loaded flake check (2026-10-21).
    let rig = Rig::with_handoff(sock::my_uid(), sock::my_uid() + 1);
    let req = rig.request("echo ran", &rig.dir);
    match hand_over(&rig.handoff, sock::my_uid()) {
        Ok(conn) => assert!(
            run_on(conn, None, &req).is_err(),
            "nobody serves this session"
        ),
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ),
            "the refusal is a closed socket, not {e}"
        ),
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!rig.log().exists(), "proot must not have been started");
}

#[test]
fn ulsh_gets_its_session_from_the_gateways_relay() {
    let gateway = sock::my_uid() + 1;
    let rig = Rig::new(gateway);
    let relay = relay_to(&rig, gateway);
    let conn = client::open_relay(&relay, sock::my_uid()).unwrap();
    let req = rig.request("echo through-the-relay", &rig.dir);
    let (code, out, _) = run_on(conn, Some(sock::my_uid()), &req).unwrap();
    assert_eq!((code, out.trim()), (0, "through-the-relay"));
}

#[test]
fn a_relay_refusal_is_the_error_ulsh_reports() {
    let rig = Rig::new(sock::my_uid());
    let relay = format!("{}-local", rig.socket);
    fake_relay(&relay, || Err("the userland package is not bound".into()));
    let err = client::open_relay(&relay, sock::my_uid()).expect_err("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    assert!(err.to_string().contains("not bound"), "{err}");
}

#[test]
fn ulsh_refuses_a_relay_that_is_not_its_own_app() {
    let rig = Rig::new(sock::my_uid());
    let relay = relay_to(&rig, sock::my_uid());
    let err = client::open_relay(&relay, sock::my_uid() + 1).expect_err("wrong relay uid");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    assert!(err.to_string().contains("not taking a session"), "{err}");
}

#[test]
fn the_ulsh_binary_takes_its_session_from_ulsh_relay() {
    let gateway = sock::my_uid() + 1;
    let rig = Rig::new(gateway);
    let relay = relay_to(&rig, gateway);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ulsh"))
        .args(["-c", "echo via-ulsh-relay; exit 4"])
        .env("ULSH_RELAY", &relay)
        // The direct socket names nothing: only the relay can work.
        .env("ULSH_SOCKET", format!("{}-nobody", rig.socket))
        .env("ULSH_BROKER_UID", sock::my_uid().to_string())
        .env("ULSH_CWD", &rig.dir)
        .env("FAKE_PROOT_LOG", rig.log())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "stderr: {stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "via-ulsh-relay"
    );
}

// ── Named sandboxes: `[sandbox] backend = "userland"` (PLAN-phone-userland.md T3 design item 4) ──

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn a_named_sandbox_is_a_copy_of_the_rootfs_and_its_writes_stay_in_it() {
    use std::os::unix::fs::symlink;
    let rig = Rig::new(sock::my_uid());
    let rootfs = rig.cfg.rootfs.clone();
    std::fs::write(rootfs.join("etc/os-release"), "NAME=\"Alpine Linux\"\n").unwrap();
    std::fs::write(rootfs.join("etc/resolv.conf"), "nameserver 1.1.1.1\n").unwrap();
    // proot's link2symlink writes absolute HOST paths into the rootfs;
    // the guest's own links are relative or guest-absolute.
    symlink(rootfs.join("etc/os-release"), rootfs.join("etc/l2s")).unwrap();
    symlink("os-release", rootfs.join("etc/relative")).unwrap();
    symlink("/etc/os-release", rootfs.join("etc/guest")).unwrap();
    std::fs::hard_link(rootfs.join("etc/os-release"), rootfs.join("etc/hard")).unwrap();
    std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    std::fs::write(rootfs.join("usr/bin/tool"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        rootfs.join("usr/bin/tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    // Sandbox A: made on first use, the guest told its name, its writes
    // (a new file, an append to a copied one) its own.
    let a = rig.sandbox("box-a");
    let req = rig.in_sandbox(
        "box-a",
        r#"echo a > "$FAKE_ROOT/etc/a-only"; echo more >> "$FAKE_ROOT/etc/os-release"; printf '%s|%s' "$VELES_SANDBOX" "$FAKE_ROOT""#,
    );
    let (code, out, err) = run(&rig, None, &req).unwrap();
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, format!("box-a|{}", a.display()));
    let argv = read(&rig.log());
    assert!(argv.contains(&format!("-r\n{}\n", a.display())), "{argv}");
    assert!(!argv.contains(":/shared"), "{argv}");
    assert!(a.join("etc/a-only").exists());
    assert_eq!(
        std::fs::read_link(a.join("etc/l2s")).unwrap(),
        a.join("etc/os-release"),
        "a link into the rootfs points into the copy"
    );
    assert_eq!(
        std::fs::read_link(a.join("etc/relative")).unwrap(),
        PathBuf::from("os-release")
    );
    assert_eq!(
        std::fs::read_link(a.join("etc/guest")).unwrap(),
        PathBuf::from("/etc/os-release")
    );
    // The rootfs's hard-link pair is still a pair inside the copy: the
    // append shows under both names there...
    assert_eq!(read(&a.join("etc/hard")), "NAME=\"Alpine Linux\"\nmore\n");
    assert_eq!(
        std::fs::metadata(a.join("usr/bin/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    // ...and none of it reached the rootfs, through a link or otherwise.
    assert!(!rootfs.join("etc/a-only").exists());
    assert_eq!(
        read(&rootfs.join("etc/os-release")),
        "NAME=\"Alpine Linux\"\n"
    );
    assert_eq!(read(&rootfs.join("etc/hard")), "NAME=\"Alpine Linux\"\n");

    // Sandbox B, made after A wrote: A's file is not there.
    let req = rig.in_sandbox(
        "box-b",
        r#"test -e "$FAKE_ROOT/etc/a-only" && echo leaked || echo apart"#,
    );
    let (code, out, _) = run(&rig, None, &req).unwrap();
    assert_eq!((code, out.trim()), (0, "apart"));
    assert!(rig.sandbox("box-b").join("etc/os-release").exists());

    // A again: kept as it was (not made anew), and the rootfs's current
    // DNS servers follow it in (the service rewrites the rootfs's at
    // each start).
    std::fs::write(rootfs.join("etc/resolv.conf"), "nameserver 9.9.9.9\n").unwrap();
    let req = rig.in_sandbox(
        "box-a",
        r#"cat "$FAKE_ROOT/etc/a-only" "$FAKE_ROOT/etc/resolv.conf""#,
    );
    let (code, out, _) = run(&rig, None, &req).unwrap();
    assert_eq!((code, out.as_str()), (0, "a\nnameserver 9.9.9.9\n"));
    // Nothing of the copying is left beside the sandboxes.
    let names: Vec<String> = std::fs::read_dir(rig.dir.join("sandboxes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, ["box-a", "box-b"], "{names:?}");
}

#[test]
fn a_sandbox_name_that_is_a_path_is_refused_before_anything_runs() {
    let rig = Rig::new(sock::my_uid());
    for bad in ["../rootfs", "a/b", ".seed-x", "", "-r"] {
        let req = rig.in_sandbox(bad, "echo ran");
        let err = run(&rig, None, &req).expect_err(bad);
        assert!(err.to_string().contains("sandbox"), "{bad:?}: {err}");
    }
    assert!(!rig.log().exists(), "proot must not have been started");
    assert!(!rig.dir.join("sandboxes").exists(), "nothing was made");
}

#[test]
fn a_broker_that_keeps_no_sandboxes_refuses_a_request_naming_one() {
    let rig = Rig::without_sandboxes();
    let err = run(&rig, None, &rig.in_sandbox("box-a", "echo ran")).expect_err("refused");
    assert!(err.to_string().contains("keeps no sandboxes"), "{err}");
    assert!(!rig.log().exists());
    // The rootfs still serves.
    let (code, out, _) = run(&rig, None, &rig.request("echo rootfs", &rig.dir)).unwrap();
    assert_eq!((code, out.trim()), (0, "rootfs"));
}

#[test]
fn a_session_in_the_rootfs_carries_no_sandbox_marker_whatever_the_client_says() {
    let rig = Rig::new(sock::my_uid());
    let mut req = rig.request(r#"printf '[%s]' "$VELES_SANDBOX""#, &rig.dir);
    req.env
        .insert(proto::SANDBOX_MARKER.into(), "box-forged".into());
    let (code, out, _) = run(&rig, None, &req).unwrap();
    assert_eq!((code, out.as_str()), (0, "[]"));
}

#[test]
fn the_ulsh_binary_runs_in_ulsh_sandbox_with_ulsh_home() {
    let gateway = sock::my_uid() + 1;
    let rig = Rig::new(gateway);
    let relay = relay_to(&rig, gateway);
    let ulsh = |sandbox: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_ulsh"))
            .args([
                "--exec",
                "/bin/sh",
                "-c",
                r#"printf '%s|%s' "$VELES_SANDBOX" "$HOME""#,
            ])
            .env("ULSH_RELAY", &relay)
            .env("ULSH_BROKER_UID", sock::my_uid().to_string())
            .env("ULSH_CWD", &rig.dir)
            .env("ULSH_SANDBOX", sandbox)
            .env("ULSH_HOME", "/work")
            .env("FAKE_PROOT_LOG", rig.log())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let out = ulsh("box-u");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "box-u|/work");
    assert!(rig.sandbox("box-u").is_dir());
    // A name that is not one stops in ulsh, before any session.
    std::fs::remove_file(rig.log()).unwrap();
    let out = ulsh("../rootfs");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ULSH_SANDBOX"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!rig.log().exists());
}
