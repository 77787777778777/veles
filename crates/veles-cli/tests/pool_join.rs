//! Joining the pool by invite, end to end (PLAN-sessions-and-phone.md
//! P2d; USER decisions D8–D10): three REAL `veles gateway run` processes
//! on loopback — an arbiter A, a member M already linked to it, and a
//! fresh node N that has nothing but the invite's text.
//!
//! A mints the invite (`veles pool invite n --text`); N joins with it
//! (`veles pool join`), which redeems at A; A introduces N to M. Then the
//! links are real: N mails a session on M with the tokens the join minted,
//! and an invite cannot be used twice.
//!
//! Hermetic: scratch homes under the per-run TMPDIR, loopback ports, a
//! stub provider in this process.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const A_ID: &str = "aaaaaaaa-0000-4000-8000-00000000000a";
const M_ID: &str = "mmmmmmmm-0000-4000-8000-00000000000m";
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

fn veles(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env_remove("VELES_SESSION_PRINCIPAL")
        .env_remove("A2A_PEER_TOKENS")
        .env_remove("A2A_BEARER_TOKEN")
        .env_remove("A2A_HOST")
        .stdin(Stdio::null());
    c
}

fn gateway(home: &Path, a2a_port: u16, desk_port: u16) -> Proc {
    Proc(
        veles(home)
            .args(["gateway", "run"])
            .env("A2A_PORT", a2a_port.to_string())
            .env("VELES_DESKTOP_PORT", desk_port.to_string())
            .env("VELES_DESKTOP_OPEN", "1")
            .stdout(std::fs::File::create(home.join("gw.log")).unwrap())
            .stderr(std::fs::File::create(home.join("gw.err")).unwrap())
            .spawn()
            .expect("spawn a gateway"),
    )
}

// Bounded dials: a connect or a read that never returns would pin
// `wait_until` past its deadline (the deadline is checked between
// probes), and on Windows a loopback SYN to a port nothing listens on
// can be black-holed by the firewall instead of refused (2026-10-13).
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

fn out(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn a_fresh_node_joins_the_whole_pool_from_one_invite() {
    let provider = stub_provider();
    let root = std::env::temp_dir().join(format!(
        "veles-pool-join-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (a_a2a, m_a2a, n_a2a) = (free_port(), free_port(), free_port());
    let (a_desk, m_desk, n_desk) = (free_port(), free_port(), free_port());

    // A, the arbiter, linked to M.
    let a = home(
        &root,
        "a",
        A_ID,
        &provider,
        &format!(
            "[node]\nendpoint = \"http://127.0.0.1:{a_a2a}\"\n\n\
             [platforms.a2a]\nenabled = true\n\n\
             [a2a_agents.m]\nurl = \"http://127.0.0.1:{m_a2a}\"\n\
             auth = {{ type = \"bearer\", token = \"a-to-m\" }}\n\n\
             [cluster]\narbiter = \"{A_ID}\"\ngossip_peers = [\"m\"]\ntask_peers = [\"m\"]\n"
        ),
        "A2A_PEER_TOKENS=m:m-to-a\n",
    );
    // M, a member, linked to A and knowing it as its arbiter.
    let m = home(
        &root,
        "m",
        M_ID,
        &provider,
        &format!(
            "[node]\nendpoint = \"http://127.0.0.1:{m_a2a}\"\n\n\
             [platforms.a2a]\nenabled = true\n\n\
             [a2a_agents.a]\nurl = \"http://127.0.0.1:{a_a2a}\"\n\
             auth = {{ type = \"bearer\", token = \"m-to-a\" }}\n\n\
             [cluster]\narbiter = \"{A_ID}\"\ngossip_peers = [\"a\"]\ntask_peers = [\"a\"]\n"
        ),
        "A2A_PEER_TOKENS=a:a-to-m\n",
    );
    // M's registry knows the arbiter's endpoint, as gossip would have.
    {
        let store = veles_core::store::Store::open(&m.join("state.db")).unwrap();
        store
            .node_register(
                A_ID,
                "a",
                "",
                "",
                "",
                None,
                Some(&format!("http://127.0.0.1:{a_a2a}")),
                None,
                None,
            )
            .unwrap();
    }
    // N: nothing but a model.
    let n = home(&root, "n", N_ID, &provider, "", "");

    let _ga = gateway(&a, a_a2a, a_desk);
    let _gm = gateway(&m, m_a2a, m_desk);
    wait_until("A and M", 90, || {
        up(a_desk) && up(m_desk) && a2a_up(a_a2a) && a2a_up(m_a2a)
    });

    // An invite is the arbiter's alone.
    let o = veles(&m)
        .args([
            "pool",
            "invite",
            "n",
            "--url",
            &format!("http://127.0.0.1:{n_a2a}"),
            "--text",
        ])
        .output()
        .unwrap();
    assert!(
        !o.status.success() && out(&o).contains("only the arbiter"),
        "{}",
        out(&o)
    );

    let o = veles(&a)
        .args([
            "pool",
            "invite",
            "n",
            "--url",
            &format!("http://127.0.0.1:{n_a2a}"),
            "--text",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", out(&o));
    // `veles` logs to stdout too: the invite is its own line.
    let invite = String::from_utf8_lossy(&o.stdout)
        .lines()
        .find(|l| l.starts_with("veles-pool:1:"))
        .unwrap_or_else(|| panic!("no invite in {}", out(&o)))
        .to_string();

    let o = veles(&n)
        .args(["pool", "join", &invite, "--yes"])
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "join: {}\nA: {}\nM: {}",
        out(&o),
        tail(&a.join("gw.err")),
        tail(&m.join("gw.err"))
    );
    assert!(
        out(&o).contains("Joined: a and 1 member(s): m"),
        "{}",
        out(&o)
    );

    // Every side of every link is written.
    let (ca, cm, cn) = (
        read(&a.join("config.toml")),
        read(&m.join("config.toml")),
        read(&n.join("config.toml")),
    );
    assert!(
        ca.contains("[a2a_agents.n]") && cm.contains("[a2a_agents.n]"),
        "A:\n{ca}\nM:\n{cm}"
    );
    assert!(
        cn.contains("[a2a_agents.a]") && cn.contains("[a2a_agents.m]"),
        "N:\n{cn}"
    );
    assert!(cn.contains(&format!("arbiter = \"{A_ID}\"")), "{cn}");
    assert!(
        cn.contains(&format!("endpoint = \"http://127.0.0.1:{n_a2a}\"")),
        "{cn}"
    );
    for (who, env) in [
        ("A", read(&a.join(".env"))),
        ("M", read(&m.join(".env"))),
        ("N", read(&n.join(".env"))),
    ] {
        assert!(env.contains("A2A_PEER_TOKENS="), "{who}: {env}");
    }
    assert!(
        read(&a.join(".env")).contains("m:m-to-a,n:"),
        "A keeps M's token"
    );
    // The bind host is node-local config now (`platforms.a2a.extra.host`,
    // beside the port), not a `.env` line: `.env` holds secrets.
    let n_cfg: toml::Value = toml::from_str(&read(&n.join("config.toml"))).unwrap();
    assert_eq!(
        n_cfg
            .get("platforms")
            .and_then(|p| p.get("a2a"))
            .and_then(|a| a.get("extra"))
            .and_then(|e| e.get("host"))
            .and_then(|h| h.as_str()),
        Some("127.0.0.1"),
        "N's bind host lands in config.toml:\n{n_cfg:#?}"
    );
    assert!(
        !read(&n.join(".env")).contains("A2A_HOST"),
        "the join writes no A2A_HOST to .env any more"
    );

    // Single use: the same invite again is refused, and writes nothing.
    let before = read(&n.join("config.toml"));
    let o = veles(&n)
        .args(["pool", "join", &invite, "--yes"])
        .output()
        .unwrap();
    assert!(
        !o.status.success() && out(&o).contains("used already"),
        "{}",
        out(&o)
    );
    assert_eq!(read(&n.join("config.toml")), before);

    // The links are real: N, now up, mails a session M holds, with the
    // tokens the join minted; M answers N's list over its own new link.
    let _gn = gateway(&n, n_a2a, n_desk);
    wait_until("N", 90, || up(n_desk) && a2a_up(n_a2a));
    let o = veles(&m)
        .args(["session", "send", "n/no-such-session", "hello from m"])
        .output()
        .unwrap();
    let said = out(&o);
    assert!(
        said.contains("no session no-such-session"),
        "M reaches N over the link the join made: {said}\nN: {}",
        tail(&n.join("gw.err"))
    );
    let o = veles(&n)
        .args(["session", "send", "m/no-such-session", "hello from n"])
        .output()
        .unwrap();
    let said = out(&o);
    assert!(
        said.contains("no session no-such-session"),
        "N reaches M over the link the join made: {said}\nM: {}",
        tail(&m.join("gw.err"))
    );
    let _ = std::fs::remove_dir_all(&root);
}
