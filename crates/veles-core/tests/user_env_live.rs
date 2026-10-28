//! Live user-env check — OPT-IN, like the IRC acl suite: needs rootless
//! podman and the `veles-sandbox:latest` image
//! (scripts/build-sandbox-image.sh builds it; scripts/sandbox_vm_check.sh
//! provisions this box). The unit suite pins argv and routing without
//! spawning anything; THIS test is where the container actually turns.
//!
//! ```sh
//! cargo test -p veles-core --test user_env_live -- --ignored --nocapture
//! ```

use std::process::Command as StdCommand;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Both tests below point `VELES_HOME` at their own scratch directory,
/// and the environment is process-global: run in parallel (cargo's
/// default, and `--ignored` does not change it) one test's `ensure`
/// resolves a workspace under the OTHER's home, the container binds that
/// directory instead, and the first test's assertions about its own
/// `workdir` fail. Two runs in three did, before this existed — and the
/// failure reads as "container writes are not host-visible", which is a
/// fence claim, not a test-harness one. The unit suite has
/// `testenv::lock()` for exactly this; it is `pub(crate)`, so an
/// integration test brings its own.
///
/// Poison-recovering, for the same reason: a failed test must not
/// cascade into the one that was waiting on it.
fn home_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn podman_ready() -> bool {
    let Ok(o) = StdCommand::new("podman")
        .arg("image")
        .arg("exists")
        .arg("veles-sandbox:latest")
        .status()
    else {
        return false;
    };
    o.success()
}

/// One podman invocation, stdout trimmed.
fn podman(args: &[&str]) -> String {
    let out = StdCommand::new("podman")
        .args(args)
        .output()
        .expect("podman runs");
    assert!(
        out.status.success(),
        "podman {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The library's warnings are part of what this suite exercises — the
/// no-rootless-driver warn below in particular — and a test binary with
/// no tracing subscriber drops them on the floor, so the one behaviour
/// `ensure` is required to have on a helper-less host (say it, then carry
/// on) would be unobservable here. `with_test_writer` keeps the output
/// out of the way unless `--nocapture` or a failure asks for it.
fn logs() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_test_writer()
            .try_init();
    });
}

// Holding a std guard across awaits is the POINT here — it is what
// serializes the two tests — and it cannot starve an executor: each
// `#[tokio::test]` builds its OWN runtime, so the test waiting on this
// lock blocks a thread of its own runtime while the holder's awaits run
// on another. (Same reasoning as the `#[allow]` in gateway.rs's tests.)
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs rootless podman + veles-sandbox:latest (see scripts/sandbox_vm_check.sh)"]
async fn user_env_round_trip() {
    if !podman_ready() {
        eprintln!("SKIP: podman or veles-sandbox:latest missing — build the image first");
        return;
    }
    logs();
    // Held for the whole test: VELES_HOME is process-global and the
    // sibling test below repoints it. See `home_lock`.
    let _home_guard = home_lock();
    // Scratch VELES_HOME: env keys hash the home, so this cannot touch
    // (or collide with) any real deployment's envs.
    let home = std::env::temp_dir().join(format!(
        "veles-userenv-live-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("VELES_HOME", &home);

    let key = veles_core::user_envs::key_for("irc:smoke");
    let name = veles_core::user_envs::container_name(&key);

    // 1. ensure: workspace + running container.
    let workdir = veles_core::user_envs::ensure(&key).await.expect("ensure");
    assert!(workdir.is_dir());
    assert_eq!(
        podman(&["inspect", "-f", "{{.State.Running}}", &name]),
        "true",
        "the env must be running after ensure"
    );

    // 2. exec_shell runs INSIDE the container (Ubuntu, not the NixOS
    // host) and writes land in the persistent workspace.
    let out = veles_core::user_envs::exec_shell(&key, "echo hi > from-exec.txt", 30)
        .await
        .expect("exec");
    assert_eq!(
        out.exit_code, 0,
        "stdout={} stderr={}",
        out.stdout, out.stderr
    );
    assert!(
        workdir.join("from-exec.txt").exists(),
        "container writes are host-visible through the workspace bind"
    );
    let os = veles_core::user_envs::exec_shell(&key, "head -1 /etc/os-release", 30)
        .await
        .unwrap();
    assert!(os.stdout.contains("Ubuntu"), "container os: {}", os.stdout);
    assert!(!os.stdout.contains("NixOS"), "that would be the HOST");

    // 3. write/read bytes through the fence.
    veles_core::user_envs::write_bytes(&key, "/work/from-fw.txt", b"framework-bytes")
        .await
        .expect("write_bytes");
    let back = veles_core::user_envs::read_bytes(&key, "/work/from-fw.txt")
        .await
        .expect("read_bytes");
    assert_eq!(back, b"framework-bytes");

    // 4. THE SYMLINK ESCAPE: a link planted in the workspace pointing
    // at an absolute host-looking path resolves INSIDE the container —
    // /etc of the container, never the host's.
    veles_core::user_envs::exec_shell(&key, "ln -sf /etc/os-release escape-link", 30)
        .await
        .unwrap();
    let esc = veles_core::user_envs::read_bytes(&key, "/work/escape-link")
        .await
        .expect("read through the link");
    let esc = String::from_utf8_lossy(&esc);
    assert!(esc.contains("Ubuntu"), "symlink read hit the container");
    assert!(
        !esc.contains("NixOS"),
        "symlink read reached the HOST — fence broken"
    );

    // 5. execute_code's safe interp inside the env: pure compute yes,
    //    exec no.
    let tcl = veles_core::user_envs::exec_tcl(&key, "puts [expr {6*7}]", 60)
        .await
        .expect("tcl");
    assert!(tcl.stdout.contains("42"), "tcl out: {:?}", tcl.stdout);
    let tcl = veles_core::user_envs::exec_tcl(&key, "puts [exec /bin/echo pwned]", 60)
        .await
        .expect("tcl");
    assert!(
        !tcl.stdout.contains("pwned"),
        "exec stays dead: {:?}",
        tcl.stdout
    );

    // 6. background runner: starts, reports, finishes with a code.
    veles_core::user_envs::ensure(&key).await.unwrap();
    let pid = veles_core::user_envs::bg_start(&key, 7, "echo bg-out; exit 5", &workdir)
        .await
        .expect("bg_start");
    let mut code = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (chunk, _, c) = veles_core::user_envs::bg_poll(&workdir, 7, 0);
        assert!(
            !chunk.contains("tampered"),
            "the runner's own output path must never look tampered: {chunk}"
        );
        if let Some(c) = c {
            code = Some(c);
            break;
        }
    }
    assert_eq!(code, Some(5), "the runner reports the real exit code");
    let (_, cursor, _) = veles_core::user_envs::bg_poll(&workdir, 7, 0);
    let (chunk, _, _) = veles_core::user_envs::bg_poll(&workdir, 7, cursor);
    let _ = pid;

    // 7. stop the container out-of-band; the next call restarts it.
    podman(&["stop", "-t", "2", &name]);
    let out = veles_core::user_envs::exec_shell(&key, "echo revived", 60)
        .await
        .expect("auto-restart");
    assert!(
        out.stdout.contains("revived"),
        "a stopped env must come back: {}",
        out.stdout
    );
    // …and the workspace survived the stop (persistence).
    let back = veles_core::user_envs::read_bytes(&key, "/work/from-fw.txt")
        .await
        .expect("workspace survives restarts");
    assert_eq!(back, b"framework-bytes");
    let _ = chunk;

    // 8. purge removes container + workspace on demand.
    veles_core::user_envs::purge(&key, true)
        .await
        .expect("purge");
    let gone = StdCommand::new("podman")
        .arg("container")
        .arg("exists")
        .arg(&name)
        .status()
        .unwrap()
        .success();
    assert!(!gone, "purge removed the container");
    assert!(!veles_core::user_envs::workdir_for(&key).exists());

    let _ = std::fs::remove_dir_all(&home);
}

/// THE ESCALATION, PINNED.
///
/// `--network host` put every below-Admin principal's box on the HOST's
/// network namespace. `desktop_api` binds 127.0.0.1 and serves the
/// operator API — ACL edits, sandbox binds, limits — and at the time it
/// had no auth at all, on the stated reasoning that "nobody else can
/// reach loopback". A `Role::User` stranger in an IRC channel *was* on
/// that loopback, so one `curl` from inside their own sandbox was a
/// promotion to owner.
///
/// The desktop API has since grown a bearer token it mints by default,
/// which closes that one door. This test stays, and stays about the
/// NETNS rather than about that route: the host's loopback also carries
/// the A2A server and every other daemon that trusts being unreachable,
/// and a fence whose safety depends on enumerating them is not a fence.
///
/// Both directions are asserted on purpose. "The probe failed" proves
/// nothing on its own — it is equally consistent with a broken probe —
/// so the same probe must SUCCEED against a deliberately `host`-networked
/// container built by the same argv builder that ships.
// Holding a std guard across awaits is the POINT here — it is what
// serializes the two tests — and it cannot starve an executor: each
// `#[tokio::test]` builds its OWN runtime, so the test waiting on this
// lock blocks a thread of its own runtime while the holder's awaits run
// on another. (Same reasoning as the `#[allow]` in gateway.rs's tests.)
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs rootless podman + veles-sandbox:latest (see scripts/sandbox_vm_check.sh)"]
async fn a_user_env_cannot_reach_the_hosts_loopback() {
    if !podman_ready() {
        eprintln!("SKIP: podman or veles-sandbox:latest missing — build the image first");
        return;
    }
    logs();
    // Held for the whole test: VELES_HOME is process-global and the
    // sibling test above repoints it. See `home_lock`.
    let _home_guard = home_lock();
    let home = std::env::temp_dir().join(format!(
        "veles-userenv-net-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("VELES_HOME", &home);

    // A real listener on the host's loopback — standing in for any
    // service that trusts being unreachable, of which the desktop API was
    // the sharpest example when this test was written (it has since grown
    // a token; the netns argument does not depend on that).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("host listener");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });

    // Tcl, not curl: the image's contract is `tclsh on PATH`, and the
    // safe interp used by execute_code has no socket command — this
    // needs the real one, which is what a `terminal` call gets.
    let probe = format!(
        "echo \"if {{[catch {{set s [socket 127.0.0.1 {port}]}}]}} \
         {{ puts NOPE }} else {{ puts YEP }}\" | tclsh"
    );

    // 1. The shipping path: `ensure` with a default config (no
    //    config.toml in this scratch home ⇒ the default posture).
    let key = veles_core::user_envs::key_for("irc:netprobe");
    let name = veles_core::user_envs::container_name(&key);
    veles_core::user_envs::ensure(&key).await.expect("ensure");
    let out = veles_core::user_envs::exec_shell(&key, &probe, 30)
        .await
        .expect("probe runs");
    assert!(
        out.stdout.contains("NOPE"),
        "a routed caller's box REACHED the host's loopback on :{port} \
         — stdout={:?} stderr={:?}",
        out.stdout,
        out.stderr
    );

    // 2. The same probe against a deliberately host-networked box,
    //    built by the shipping argv builder. This must CONNECT — it is
    //    what proves step 1 measured isolation and not a dead probe.
    let host_name = format!("{name}-hostnet");
    let workdir = veles_core::user_envs::workdir_for(&key);
    let cfg = veles_core::user_envs::EnvCfg {
        net: veles_core::user_envs::EnvNet::Host,
        ..veles_core::user_envs::EnvCfg::from_section(&veles_core::config::SandboxSection::default())
    };
    let mut create =
        veles_core::user_envs::build_create_command("podman", &host_name, &workdir, &cfg);
    assert!(create.status().expect("podman create").success());
    podman(&["start", &host_name]);
    let out = StdCommand::new("podman")
        .args(["exec", &host_name, "sh", "-c", &probe])
        .output()
        .expect("probe the host-networked box");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    podman(&["rm", "-f", &host_name]);
    assert!(
        said.contains("YEP"),
        "the probe could not reach the listener even with --network host, \
         so step 1 proved nothing: {said:?}"
    );

    // 3. THE UPGRADE PATH. A deployment that has been running already
    //    has one host-networked box per channel, and a container keeps
    //    its namespace for as long as it lives — so a fix that only
    //    applies to NEW boxes leaves the hole open for exactly the
    //    people who already used the bot. Stand up a box under the env's
    //    real name with the old posture, prove it is reachable, then let
    //    `ensure` see it.
    veles_core::user_envs::purge(&key, true)
        .await
        .expect("purge");
    let workdir = veles_core::user_envs::workdir_for(&key);
    std::fs::create_dir_all(&workdir).unwrap();
    let mut create = veles_core::user_envs::build_create_command("podman", &name, &workdir, &cfg);
    assert!(create.status().expect("podman create").success());
    podman(&["start", &name]);
    let legacy = StdCommand::new("podman")
        .args(["exec", &name, "sh", "-c", &probe])
        .output()
        .expect("probe the legacy box");
    assert!(
        String::from_utf8_lossy(&legacy.stdout).contains("YEP"),
        "the legacy box was supposed to be reachable — the premise of this step"
    );

    veles_core::user_envs::ensure(&key)
        .await
        .expect("re-ensure");
    let out = veles_core::user_envs::exec_shell(&key, &probe, 30)
        .await
        .expect("probe after ensure");
    assert!(
        out.stdout.contains("NOPE"),
        "an existing host-networked box SURVIVED the tightened posture — \
         upgrading would not close the hole for anyone already using the bot: \
         stdout={:?}",
        out.stdout
    );

    veles_core::user_envs::purge(&key, true)
        .await
        .expect("purge");
    let _ = std::fs::remove_dir_all(&home);
}

/// THE OTHER HALF OF THE NETNS PROBE: an `isolated` box must really have
/// outbound.
///
/// The sibling test proves a routed caller's box cannot reach the HOST's
/// loopback — and that can pass for the wrong reason, because a box with
/// no network at all cannot reach loopback either. `EnvNet::Isolated` is
/// documented as "a namespace of its own, WITH outbound", and on a
/// rootless podman that outbound is pasta's or slirp4netns' job. A host
/// with neither still creates the box (measured: `podman run` exits 0),
/// so `isolated` silently becomes `none` with extra steps while
/// `net_mode_matches` reports the configured posture as satisfied. This
/// is the assertion that tells the two apart on a real host, and the
/// reason `ensure` warns and `veles doctor --sandbox` reports.
///
/// The TARGET is `host.containers.internal`, and that is measured rather
/// than preferred. The obvious design — the box connects to the host's
/// outward address, found with a UDP `connect` and `local_addr` — does
/// not work under pasta, which mirrors the host's addresses INTO the box:
/// the box here owns the host's own 192.168.1.50, so a connect to that
/// address is delivered locally and was refused, while the same listener
/// accepted a host-side connection immediately. podman writes
/// `host.containers.internal` into every container's /etc/hosts for
/// exactly this reason (169.254.1.2 under pasta, 10.0.2.2 under
/// slirp4netns), so the NAME travels across drivers where the address
/// does not.
///
/// The LISTENER binds the wildcard. Binding the outward address works
/// too — pasta forwards the name-based connection into the host namespace
/// and a listener on that address accepts it, which was checked rather
/// than assumed — but the wildcard needs no discovery step, so there is
/// one less thing to fail and nothing to SKIP on.
// Holding a std guard across awaits: see the sibling test.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs rootless podman + veles-sandbox:latest (see scripts/sandbox_vm_check.sh)"]
async fn an_isolated_box_really_has_outbound() {
    use std::io::Read as _;

    if !podman_ready() {
        eprintln!("SKIP: podman or veles-sandbox:latest missing — build the image first");
        return;
    }
    logs();
    let _home_guard = home_lock();

    // Honest SKIP, printed: a host with no rootless network driver cannot
    // give an isolated box outbound at all, and that is the warning's
    // subject, not a fence regression. Failing the suite for it would
    // teach whoever runs it to ignore the failure that matters.
    let net = veles_core::user_envs::rootless_net_state().await;
    if !net.isolated_has_outbound() {
        eprintln!(
            "SKIP: this host cannot give an isolated box outbound — {}",
            net.describe()
        );
        return;
    }
    eprintln!("host rootless networking: {}", net.describe());

    let home = std::env::temp_dir().join(format!(
        "veles-userenv-outbound-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("VELES_HOME", &home);

    // One wildcard listener for both phases, keeping every marker it is
    // sent so phase 2 can prove that nothing arrived.
    let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("host listener");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            let _ = tx.send(buf.trim().to_string());
        }
    });

    // Tcl, single-quoted so the shell leaves `$s` alone, and `timeout`
    // around tclsh because a `socket` to nowhere can block: the phase
    // that is EXPECTED to fail must fail by saying so, not by hanging the
    // suite.
    let probe = |target: &str| {
        format!(
            "echo 'if {{[catch {{set s [socket {target} {port}]}} e]}} {{ puts NOPE }} \
             else {{ puts $s YEP; flush $s; close $s; puts SENT }}' | timeout 25 tclsh"
        )
    };

    // 1. The shipping path: `ensure` with a default config, i.e. the
    //    posture every routed caller's box actually gets.
    let key = veles_core::user_envs::key_for("irc:outbound");
    veles_core::user_envs::ensure(&key).await.expect("ensure");
    let out = veles_core::user_envs::exec_shell(&key, &probe("host.containers.internal"), 60)
        .await
        .expect("probe runs");
    assert!(
        out.stdout.contains("SENT"),
        "the default `isolated` box could not reach the host at all — outbound is not \
         working, so `isolated` is `none` with extra steps: stdout={:?} stderr={:?}",
        out.stdout,
        out.stderr
    );
    let heard = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the box said SENT but the host listener heard nothing");
    assert_eq!(
        heard, "YEP",
        "the connection that arrived carried something else: {heard:?}"
    );

    // 2. The same probe against a deliberately `none` box, built by the
    //    shipping argv builder. It must NOT connect. Without this step a
    //    passing phase 1 is consistent with a probe that would pass in a
    //    box with no network — which is the vacuous result this test
    //    exists to rule out, and the mirror of the sibling test proving
    //    its own loopback probe is not dead.
    let none_name = format!("{}-nonet", veles_core::user_envs::container_name(&key));
    let workdir = veles_core::user_envs::workdir_for(&key);
    let cfg = veles_core::user_envs::EnvCfg {
        net: veles_core::user_envs::EnvNet::None,
        ..veles_core::user_envs::EnvCfg::from_section(&veles_core::config::SandboxSection::default())
    };
    let mut create =
        veles_core::user_envs::build_create_command("podman", &none_name, &workdir, &cfg);
    assert!(create.status().expect("podman create").success());
    podman(&["start", &none_name]);
    let out = StdCommand::new("podman")
        .args([
            "exec",
            &none_name,
            "sh",
            "-c",
            &probe("host.containers.internal"),
        ])
        .output()
        .expect("probe the network-less box");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    podman(&["rm", "-f", &none_name]);
    assert!(
        said.contains("NOPE") && !said.contains("SENT"),
        "a box with --network none REACHED the host, so phase 1 measured nothing: {said:?} \
         stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(500))
            .is_err(),
        "the network-less box's marker arrived at the host listener"
    );

    veles_core::user_envs::purge(&key, true)
        .await
        .expect("purge");
    let _ = std::fs::remove_dir_all(&home);
}
