//! Session mail between two REAL pool nodes (PLAN-sessions-and-phone.md,
//! round S-b): two `veles gateway run` processes on their own homes and
//! ports, talking A2A with real bearer tokens. Node A's command line
//! mails a session node B holds; B runs it as a User-level turn (what an
//! A2A peer holds by default) and A gets the answer.
//!
//! Hermetic: scratch homes under the per-run TMPDIR, loopback ports, a
//! stub provider in this process.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REPLY: &str = "node b answered";

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
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\",\"content\":\"{REPLY}\"}}}}]}}\n\ndata: [DONE]\n\n"
                );
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

/// One node's home: the stub provider, A2A on, the other node as its
/// `[a2a_agents]` peer and its task peer, and the tokens.
fn node_home(
    root: &Path,
    name: &str,
    provider: &str,
    peer: &str,
    peer_port: u16,
    my_token: &str,
    peer_token: &str,
) -> PathBuf {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{provider}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n\n\
             [platforms.a2a]\nenabled = true\n\n\
             [a2a_agents.{peer}]\nurl = \"http://127.0.0.1:{peer_port}\"\n\
             auth = {{ type = \"bearer\", token = \"{my_token}\" }}\n\n\
             [cluster]\ntask_peers = [\"{peer}\"]\n"
        ),
    )
    .unwrap();
    // The token the PEER presents to us, and the name we know it by.
    std::fs::write(
        d.join(".env"),
        format!("A2A_PEER_TOKENS={peer}:{peer_token}\nA2A_BEARER_TOKEN={my_token}\n"),
    )
    .unwrap();
    d
}

fn veles(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env_remove("VELES_SESSION_PRINCIPAL")
        // The A2A vars this home's own `.env` sets must reach the child
        // UNSHADOWED. Secrets resolve ambient env first (authn::env_or_dotenv),
        // so an operator's real A2A_HOST — veles loads .env into its process
        // env at boot, which is every agent session on a laptop with a
        // WireGuard address — makes the gateway bind A2A THERE while this test
        // dials 127.0.0.1, and node B looks like it never came up. Same
        // removals pool_join.rs has; a token from the ambient env would
        // likewise authenticate the wrong peer.
        .env_remove("A2A_HOST")
        .env_remove("A2A_PEER_TOKENS")
        .env_remove("A2A_BEARER_TOKEN")
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

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    // `connect_timeout`, not `connect`: on Windows a loopback SYN can sit
    // unanswered for minutes (PLAN-windows.md §3 notes, 2026-10-13); a bounded
    // wait keeps the retry loop alive instead of hanging the test.
    let Ok(mut s) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(5),
    ) else {
        return (0, String::new());
    };
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(req.as_bytes());
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (
        status,
        out.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
    )
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!("timed out waiting for {what}");
}

fn open_events(port: u16, sid: &str) -> TcpStream {
    let mut s = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(5),
    )
    .expect("connect");
    let req = format!(
        "GET /api/sessions/{sid}/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&got).contains("session-live") {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    s
}

fn tail(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(25)..].join("\n")
}

#[test]
fn node_a_mails_a_session_node_b_holds_and_gets_the_answer() {
    let url = stub_provider();
    let root = std::env::temp_dir().join(format!(
        "veles-mailnet-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (a2a_a, a2a_b) = (free_port(), free_port());
    let (desk_a, desk_b) = (free_port(), free_port());
    // A calls B with tok-a; B knows tok-a as "a". And the other way.
    let home_a = node_home(&root, "a", &url, "b", a2a_b, "tok-a", "tok-b");
    let home_b = node_home(&root, "b", &url, "a", a2a_a, "tok-b", "tok-a");
    let _ga = gateway(&home_a, a2a_a, desk_a);
    let _gb = gateway(&home_b, a2a_b, desk_b);
    for (port, home) in [(desk_a, &home_a), (desk_b, &home_b)] {
        wait_until("a gateway's health route", 60, || {
            http(port, "GET", "/api/health", "").0 == 200
        });
        let _ = home;
    }
    wait_until("B's A2A server", 60, || {
        http(a2a_b, "GET", "/.well-known/agent-card.json", "").0 == 200
    });

    // A session on B that a window shows.
    let (code, body) = http(desk_b, "POST", "/api/sessions/new", r#"{"title":"on b"}"#);
    assert_eq!(code, 201, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sid = v["session_id"].as_str().unwrap().to_string();
    let _window = open_events(desk_b, &sid);

    // A sees it across the link.
    let out = veles(&home_a)
        .args(["session", "list", "--remote"])
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        list.contains(&format!("b/{sid}")),
        "A lists B's session as b/<id>:\n{list}\n--- b\n{}",
        tail(&home_b.join("gw.log"))
    );

    // A mails it and waits for the answer.
    let out = veles(&home_a)
        .args([
            "session",
            "send",
            &format!("b/{sid}"),
            "hello from a",
            "--wait",
            "60",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "send exited {:?}\n{stdout}\n{}\n--- a\n{}\n--- b\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        tail(&home_a.join("gw.log")),
        tail(&home_b.join("gw.log"))
    );
    assert!(stdout.contains(REPLY), "{stdout}");

    // On B: the mail came through peer "a", at a User's rights.
    let store = veles_core::store::Store::open(&home_b.join("state.db")).unwrap();
    let mail = store.mail_for_session(&sid, 5).unwrap();
    let row = mail
        .iter()
        .find(|r| r.via_peer.as_deref() == Some("a"))
        .expect("a mail via peer a");
    assert_eq!(row.state, "answered");
    let p: veles_core::acl::Principal =
        serde_json::from_str(row.from_principal.as_deref().unwrap()).unwrap();
    assert_eq!(
        p.role,
        veles_core::acl::Role::User,
        "a peer is a User by default"
    );
    let hist = store.load_history(&sid).unwrap();
    assert!(hist
        .iter()
        .any(|m| m.injection.as_deref() == Some("origin:mail") && m.content.contains("on node a")));

    // `session mail b/<id>` asks B for it.
    let out = veles(&home_a)
        .args(["session", "mail", &format!("b/{}", row.id)])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        shown.contains("answered") && shown.contains(REPLY),
        "{shown}"
    );

    // A node A does not know is named, not guessed.
    let out = veles(&home_a)
        .args(["session", "send", &format!("nowhere/{sid}"), "x"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nowhere"));

    let _ = std::fs::remove_dir_all(&root);
}
