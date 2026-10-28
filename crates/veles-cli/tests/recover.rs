//! A node whose home was removed comes back from its pool peer with one
//! command (PLAN-recover.md): two scratch homes on loopback, a REAL
//! `veles gateway run` for the arbiter A, and a node N that only ever ran
//! the CLI.
//!
//! N lives first: two chat sessions, a memory file, its config, all
//! shipped to A through the real transports (`replicas push`, `memory
//! push`, `configsync push`). Then N's home is deleted. A mints a
//! recovery invite (`veles pool invite n --recover`), and on the empty
//! home `veles recover '<invite>' --as n` takes N's id back and pulls its
//! sessions, memory, config, A's skills and commands, and sends the vault
//! a join request. What comes back is checked on disk; so is the link
//! rotation on both sides, the guard against a running gateway, an
//! idempotent second run — and that N's NEXT store push lands in A's
//! replica instead of being skipped as already applied (the
//! rebuild-replica bug of 2026-10-19).
//!
//! Hermetic: scratch homes under the per-run TMPDIR, loopback ports, a
//! stub provider in this process, and every veles child pointed away
//! from a desktop port a real gateway on this machine may hold.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const A_ID: &str = "aaaaaaaa-0000-4000-8000-00000000000a";
const N_ID: &str = "nnnnnnnn-0000-4000-8000-00000000000n";

fn stub_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut raw = Vec::new();
                let mut buf = [0u8; 16384];
                let mut need: Option<usize> = None;
                loop {
                    let n = match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                    if need.is_none() {
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                            let len = head
                                .split("content-length:")
                                .nth(1)
                                .and_then(|t| t.split(['\r', '\n']).next())
                                .and_then(|t| t.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            need = Some(end + 4 + len);
                        }
                    }
                    if matches!(need, Some(t) if raw.len() >= t) {
                        break;
                    }
                }
                let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
                let _ = s.flush();
            });
        }
    });
    format!("http://{addr}/v1")
}

/// Something answering like a gateway's desktop API: what the restore
/// guard (and so the recovery's) probes for.
fn fake_desktop_api() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
        }
    });
    port
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A veles child in `home`. The desktop port is one nothing listens on:
/// the recovery refuses while a gateway answers there, and the default
/// (8642) is where a developer's own gateway runs.
fn veles(home: &Path, dead_port: u16) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env("VELES_DESKTOP_PORT", dead_port.to_string())
        .env_remove("VELES_SESSION_PRINCIPAL")
        .env_remove("A2A_PEER_TOKENS")
        .env_remove("A2A_BEARER_TOKEN")
        .env_remove("A2A_HOST")
        .stdin(Stdio::null());
    c
}

fn gateway(home: &Path, a2a_port: u16, desk_port: u16) -> Proc {
    Proc(
        veles(home, desk_port)
            .args(["gateway", "run"])
            .env("A2A_PORT", a2a_port.to_string())
            .env("VELES_DESKTOP_OPEN", "1")
            .stdout(std::fs::File::create(home.join("gw.log")).unwrap())
            .stderr(std::fs::File::create(home.join("gw.err")).unwrap())
            .spawn()
            .expect("spawn a gateway"),
    )
}

const PROBE: Duration = Duration::from_secs(5);

fn up(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, PROBE) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(PROBE));
    let _ = s.set_write_timeout(Some(PROBE));
    let _ = s.write_all(b"GET /api/health HTTP/1.0\r\nHost: x\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.contains(" 200 ")
}

fn a2a_up(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, PROBE).is_ok()
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn tail(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn out(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Run and require success, with the arbiter's log on failure.
fn ok(c: &mut Command, what: &str, a: &Path) -> String {
    let o = c.output().unwrap();
    let said = out(&o);
    assert!(
        o.status.success(),
        "{what}: {said}\nA: {}",
        tail(&a.join("gw.err"))
    );
    said
}

fn home(root: &Path, name: &str, id: &str, provider: &str, extra: &str, env: &str) -> PathBuf {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("node.toml"),
        format!("id = \"{id}\"\nname = \"{name}\"\n"),
    )
    .unwrap();
    std::fs::write(
        d.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{provider}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n\n\
             {extra}"
        ),
    )
    .unwrap();
    std::fs::write(d.join(".env"), env).unwrap();
    d
}

fn query_i64(db: &Path, sql: &str) -> i64 {
    let s = veles_core::store::Store::open_replica_ro(db).expect("open a store read-only");
    s.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn token_in(cfg: &str, peer: &str) -> String {
    let v: toml::Value = toml::from_str(cfg).unwrap();
    v["a2a_agents"][peer]["auth"]["token"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn a_node_whose_home_was_removed_comes_back_from_its_peer() {
    let provider = stub_provider();
    let root = std::env::temp_dir().join(format!(
        "veles-recover-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (a_a2a, n_a2a, a_desk, dead) = (free_port(), free_port(), free_port(), free_port());

    // A: the arbiter, which keeps N's replica, memory and config.
    let a = home(
        &root,
        "a",
        A_ID,
        &provider,
        &format!(
            "[node]\nendpoint = \"http://127.0.0.1:{a_a2a}\"\n\n\
             [platforms.a2a]\nenabled = true\n\n\
             [a2a_agents.n]\nurl = \"http://127.0.0.1:{n_a2a}\"\n\
             auth = {{ type = \"bearer\", token = \"a-to-n\" }}\n\n\
             [cluster]\narbiter = \"{A_ID}\"\ngossip_peers = [\"n\"]\nmemsync_peers = [\"n\"]\n\
             storesync_peers = [\"n\"]\nstoresync_auto = true\nmemsync_auto = true\n"
        ),
        "A2A_PEER_TOKENS=n:n-to-a\n",
    );
    // A's own skill and command: what a recovery copies (they are per node).
    std::fs::create_dir_all(a.join("skills/probe-skill")).unwrap();
    std::fs::write(
        a.join("skills/probe-skill/SKILL.md"),
        "---\nname: probe-skill\ndescription: a probe\n---\nprobe\n",
    )
    .unwrap();
    std::fs::create_dir_all(a.join("commands")).unwrap();
    std::fs::write(a.join("commands/probe-cmd.md"), "probe $ARGUMENTS\n").unwrap();
    // N: a member of A's pool, with a config key of its own.
    let n = home(
        &root,
        "n",
        N_ID,
        &provider,
        &format!(
            "[agent]\ntts_voice = \"recovered-voice\"\n\n\
             [a2a_agents.a]\nurl = \"http://127.0.0.1:{a_a2a}\"\n\
             auth = {{ type = \"bearer\", token = \"n-to-a\" }}\n\n\
             [cluster]\narbiter = \"{A_ID}\"\nmemsync_peers = [\"a\"]\nstoresync_peers = [\"a\"]\n"
        ),
        "A2A_PEER_TOKENS=a:a-to-n\n",
    );

    let _ga = gateway(&a, a_a2a, a_desk);
    wait_until("A", 90, || up(a_desk) && a2a_up(a_a2a));
    ok(
        veles(&a, dead).args(["vault", "init"]),
        "vault init on A",
        &a,
    );

    // ── N lives: sessions, memory, config, all shipped to A ──
    for q in ["the first session before the loss", "a second one"] {
        ok(
            veles(&n, dead).args(["chat", "-Q", "-q", q]),
            "chat on N",
            &a,
        );
    }
    let probe = n.join("probe.md");
    std::fs::write(&probe, "the memory that must come back\n").unwrap();
    ok(
        veles(&n, dead).args([
            "memory",
            "write",
            "notes/recovery-probe.md",
            "--file",
            probe.to_str().unwrap(),
        ]),
        "memory write on N",
        &a,
    );
    ok(
        veles(&n, dead).args(["memory", "push", "a"]),
        "memory push",
        &a,
    );
    let a_url = format!("http://127.0.0.1:{a_a2a}");
    ok(
        veles(&n, dead).args(["configsync", "scan"]),
        "configsync scan",
        &a,
    );
    ok(
        veles(&n, dead).args(["configsync", "push", &a_url, "--bearer", "n-to-a"]),
        "configsync push",
        &a,
    );
    ok(
        veles(&n, dead).args(["replicas", "install-log"]),
        "install-log",
        &a,
    );
    let said = ok(
        veles(&n, dead).args([
            "replicas", "push", &a_url, "--since", "0", "--bearer", "n-to-a",
        ]),
        "replicas push",
        &a,
    );
    let replica = a.join("replicas").join(format!("{N_ID}.db"));
    assert_eq!(
        query_i64(&replica, "SELECT COUNT(*) FROM sessions"),
        2,
        "A holds N's two sessions: {said}"
    );
    let n_messages = query_i64(&replica, "SELECT COUNT(*) FROM messages");
    assert!(n_messages >= 4, "{n_messages}");
    let cursor = query_i64(
        &replica,
        "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'veles.applied_seq'",
    );
    let n_rows_on_a = query_i64(
        &a.join("state.db"),
        &format!("SELECT COUNT(*) FROM config_log WHERE node = '{N_ID}'"),
    );
    assert!(n_rows_on_a > 0, "A logged N's config rows");
    let n_max_seq_on_a = query_i64(
        &a.join("state.db"),
        &format!("SELECT MAX(node_seq) FROM config_log WHERE node = '{N_ID}'"),
    );

    // ── the loss ──
    std::fs::remove_dir_all(&n).unwrap();
    std::fs::create_dir_all(&n).unwrap();

    // ── the arbiter mints a recovery invite ──
    let said = ok(
        veles(&a, dead).args(["pool", "invite", "n", "--recover", "--text"]),
        "invite --recover",
        &a,
    );
    let invite = said
        .lines()
        .find(|l| l.starts_with("veles-pool:1:"))
        .unwrap_or_else(|| panic!("no invite in {said}"))
        .to_string();

    // A gateway answering on this home: refused, nothing written.
    let busy = fake_desktop_api();
    let o = veles(&n, busy)
        .args(["recover", &invite, "--as", "n", "--yes"])
        .output()
        .unwrap();
    assert!(
        !o.status.success() && out(&o).contains("gateway is running"),
        "{}",
        out(&o)
    );
    assert!(!n.join("node.toml").exists(), "the guard writes nothing");

    // A re-run needs an identity already here: on the empty home it is
    // refused, and mints none.
    let o = veles(&n, dead)
        .args(["recover", "--from", "a", "--yes"])
        .output()
        .unwrap();
    assert!(
        !o.status.success() && out(&o).contains("no identity"),
        "{}",
        out(&o)
    );
    assert!(
        !n.join("node.toml").exists(),
        "a refused re-run mints nothing"
    );

    // Without --as, a recovery invite is not taken as a new node.
    let o = veles(&n, dead)
        .args(["recover", &invite, "--yes"])
        .output()
        .unwrap();
    assert!(
        !o.status.success() && out(&o).contains("--as n"),
        "{}",
        out(&o)
    );

    // ── one command on the empty home ──
    let said = ok(
        veles(&n, dead).args(["recover", &invite, "--as", "n", "--yes"]),
        "recover",
        &a,
    );
    // The run's own account (captured; shown with --nocapture or on failure).
    println!("{said}");

    // Its identity: the old id, not a new one.
    let node_toml = read(&n.join("node.toml"));
    assert!(node_toml.contains(N_ID), "{node_toml}\n{said}");
    // Its sessions, under that id, from A's replica.
    let db = n.join("state.db");
    assert_eq!(
        query_i64(
            &db,
            &format!("SELECT COUNT(*) FROM sessions WHERE node = '{N_ID}'")
        ),
        2,
        "{said}"
    );
    assert_eq!(query_i64(&db, "SELECT COUNT(*) FROM messages"), n_messages);
    assert_eq!(
        query_i64(&db, "SELECT COUNT(*) FROM messages_fts"),
        n_messages,
        "indexed once"
    );
    // Its memory.
    let m = ok(
        veles(&n, dead).args(["memory", "cat", "notes/recovery-probe.md"]),
        "memory cat",
        &a,
    );
    assert!(m.contains("the memory that must come back"), "{m}");
    // Its config: the key it had, and its own history numbered on.
    let cfg = read(&n.join("config.toml"));
    assert!(cfg.contains("recovered-voice"), "{cfg}\n{said}");
    assert!(
        query_i64(
            &db,
            &format!("SELECT COUNT(*) FROM config_log WHERE node = '{N_ID}'")
        ) > n_rows_on_a,
        "N's history is back and its new writes are logged after it"
    );
    assert!(
        query_i64(
            &db,
            &format!("SELECT MAX(node_seq) FROM config_log WHERE node = '{N_ID}'")
        ) > n_max_seq_on_a
    );
    // A's skill and command, copied.
    assert!(n.join("skills/probe-skill/SKILL.md").is_file(), "{said}");
    assert!(n.join("commands/probe-cmd.md").is_file(), "{said}");
    // The links: rotated on both sides, the old tokens gone.
    let (cn, ca) = (cfg.clone(), read(&a.join("config.toml")));
    let (n_to_a, a_to_n) = (token_in(&cn, "a"), token_in(&ca, "n"));
    assert!(!n_to_a.is_empty() && n_to_a != "n-to-a", "{cn}");
    assert!(!a_to_n.is_empty() && a_to_n != "a-to-n", "{ca}");
    let (en, ea) = (read(&n.join(".env")), read(&a.join(".env")));
    assert!(en.contains(&format!("A2A_PEER_TOKENS=a:{a_to_n}")), "{en}");
    assert!(
        ea.contains(&format!("n:{n_to_a}")) && !ea.contains("n-to-a"),
        "{ea}"
    );
    // The vault: N's join request waits on A for a member's admit.
    let v = ok(veles(&a, dead).args(["vault", "inbox"]), "vault inbox", &a);
    assert!(v.contains(N_ID) || v.contains(" n "), "{v}");
    assert!(said.contains("veles vault admit"), "{said}");
    // And it says what it could not bring back.
    assert!(
        said.contains("NOT restored") && said.contains("node-local config keys"),
        "{said}"
    );

    // ── again: nothing doubles ──
    let again = ok(
        veles(&n, dead).args(["recover", "--from", "a", "--yes"]),
        "recover again",
        &a,
    );
    assert_eq!(
        query_i64(&db, "SELECT COUNT(*) FROM messages"),
        n_messages,
        "{again}"
    );
    assert_eq!(query_i64(&db, "SELECT COUNT(*) FROM sessions"), 2);
    assert!(again.contains("already here"), "{again}");

    // ── and the next push lands ──
    // The store log goes on after A's cursor, so N's new rows are past
    // it: applied, not skipped as already up to date.
    ok(
        veles(&n, dead).args(["chat", "-Q", "-q", "the first session after"]),
        "chat after",
        &a,
    );
    ok(
        veles(&n, dead).args(["replicas", "install-log"]),
        "install-log after",
        &a,
    );
    assert!(
        query_i64(&db, "SELECT MIN(seq) FROM store_log") > cursor,
        "N's log goes on after A's cursor {cursor}"
    );
    let said = ok(
        veles(&n, dead).args([
            "replicas", "push", &a_url, "--since", "0", "--bearer", &n_to_a,
        ]),
        "replicas push after",
        &a,
    );
    assert_eq!(
        query_i64(&replica, "SELECT COUNT(*) FROM sessions"),
        3,
        "A's replica took N's post-recovery session: {said}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
