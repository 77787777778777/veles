//! Restart recovery between REAL processes: the `veles` binary as a
//! gateway, stopped in the middle of a desktop turn that a queue row
//! started, then started again over the same state.
//!
//! What it pins (the unit tests forge a dead owner; this one has a real
//! one): the interrupted turn is continued once from saved history and
//! marked recovered, its prompt reaches the transcript once — the queue
//! row it came from is not replayed as a new turn — and the turn before
//! it is untouched.
//!
//! Hermetic: a scratch VELES_HOME under the per-run TMPDIR, its own
//! desktop port, a provider that is a TcpListener in this process and
//! holds the requests it is told to hold. No network, no key.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPLY: &str = "answered by the stub";

/// Which prompts the stub holds (a request whose LAST user message
/// contains one of these markers gets no answer until it is released),
/// and which ones have arrived.
#[derive(Default)]
struct Stub {
    held: Vec<String>,
    arrived: Vec<String>,
}

fn last_user_text(body: &str) -> String {
    let Some(start) = body.find("\r\n\r\n") else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body[start + 4..]) else {
        return String::new();
    };
    v["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
        .map(|m| match &m["content"] {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn stub_provider(state: Arc<Mutex<Stub>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let state = state.clone();
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
                let last = last_user_text(&String::from_utf8_lossy(&raw));
                let marker = {
                    let mut st = state.lock().unwrap();
                    st.arrived.push(last.clone());
                    st.held.iter().find(|m| last.contains(m.as_str())).cloned()
                };
                // Held: wait for the release (or for the gateway to go
                // away, which closes the socket under us).
                if let Some(marker) = marker {
                    let deadline = Instant::now() + Duration::from_secs(120);
                    while Instant::now() < deadline && state.lock().unwrap().held.contains(&marker)
                    {
                        std::thread::sleep(Duration::from_millis(50));
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

fn scratch_home(base_url: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-restart-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{base_url}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n"
        ),
    )
    .unwrap();
    d
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills its child on the way out, pass or fail.
struct Proc {
    child: Child,
    log: PathBuf,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn log_tail(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

fn gateway(home: &Path, port: u16, tag: &str) -> Proc {
    let log = home.join(format!("gw-{tag}.log"));
    let child = Command::new(env!("CARGO_BIN_EXE_veles"))
        .env("VELES_HOME", home)
        .env("HOME", home)
        .env_remove("VELES_SESSION_PRINCIPAL")
        .env_remove("DESKTOP_BEARER_TOKEN")
        // An operator's ambient A2A identity must not reach the scratch
        // gateway (same removals as session_mail_net.rs / pool_join.rs).
        .env_remove("A2A_HOST")
        .env_remove("A2A_PORT")
        .env_remove("A2A_PEER_TOKENS")
        .env_remove("A2A_BEARER_TOKEN")
        .env("VELES_DESKTOP_PORT", port.to_string())
        .env("VELES_DESKTOP_OPEN", "1")
        .args(["gateway", "run"])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&log).unwrap())
        .stderr(std::fs::File::create(home.join(format!("gw-{tag}.err"))).unwrap())
        .spawn()
        .expect("spawn the gateway");
    let gw = Proc { child, log };
    wait_until("the gateway's health route", 60, || {
        TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(5),
        )
        .is_ok()
            && http(port, "GET", "/api/health", "").0 == 200
    });
    gw
}

/// Stop the way a service manager does: SIGTERM on unix (the graceful
/// path that drops in-flight turn futures), TerminateProcess on Windows.
fn stop(gw: &mut Proc) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &gw.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if matches!(gw.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = gw.child.kill();
    let _ = gw.child.wait();
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(5),
    )
    .expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
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

fn store(home: &Path) -> veles_core::store::Store {
    veles_core::store::Store::open(&home.join("state.db")).expect("state.db")
}

fn user_rows(home: &Path, sid: &str, text: &str) -> usize {
    store(home)
        .load_history(sid)
        .unwrap()
        .iter()
        .filter(|m| m.role == "user" && m.injection.is_none() && m.content == text)
        .count()
}

#[test]
fn a_gateway_stopped_mid_queued_turn_continues_it_once_after_restart() {
    const A: &str = "HOLD-A the first request";
    const B: &str = "HOLD-B the queued request";
    let state = Arc::new(Mutex::new(Stub {
        held: vec!["HOLD-A".into(), "HOLD-B".into()],
        arrived: Vec::new(),
    }));
    let url = stub_provider(state.clone());
    let home = scratch_home(&url);
    let port = free_port();
    let mut gw = gateway(&home, port, "first");

    let (code, body) = http(port, "POST", "/api/sessions/new", r#"{"title":"restart"}"#);
    assert_eq!(code, 201, "sessions/new: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sid = v["session_id"].as_str().expect("session id").to_string();
    let chat = |text: &str| {
        http(
            port,
            "POST",
            "/api/chat",
            &serde_json::json!({"session_id": sid, "text": text}).to_string(),
        )
    };

    // A runs and is held at the provider; B arrives behind it and queues.
    let (code, body) = chat(A);
    assert_eq!(code, 202, "{body}");
    wait_until("A at the provider", 60, || {
        state
            .lock()
            .unwrap()
            .arrived
            .iter()
            .any(|t| t.contains("HOLD-A"))
    });
    let (code, body) = chat(B);
    assert_eq!(code, 202, "{body}");
    assert!(body.contains("queued"), "B queues behind A: {body}");

    // A finishes; the drain takes B's row and B is held mid-turn.
    state.lock().unwrap().held.retain(|m| m != "HOLD-A");
    wait_until("B at the provider", 60, || {
        state
            .lock()
            .unwrap()
            .arrived
            .iter()
            .any(|t| t.contains("HOLD-B"))
    });
    stop(&mut gw);
    {
        let st = store(&home);
        let checkpoint = st.turn_checkpoint(&sid).unwrap().expect("a checkpoint");
        assert_eq!(
            checkpoint.state,
            "running",
            "a stopped turn leaves its checkpoint\n{}",
            log_tail(&gw.log)
        );
        let queue = st.queue_items(&sid).unwrap();
        assert!(
            queue.iter().any(|q| q.body == B && q.state == "running"),
            "B's queue row was mid-turn: {queue:?}"
        );
    }
    drop(gw);

    // Restart with nothing held: B is continued, not sent again.
    state.lock().unwrap().held.clear();
    let gw2 = gateway(&home, port, "second");
    // Recovered — or replayed: a replay runs B as a NEW turn, whose
    // checkpoint replaces the recovered one, so wait for either.
    wait_until("the recovered checkpoint (or a replay)", 90, || {
        user_rows(&home, &sid, B) > 1
            || store(&home)
                .turn_checkpoint(&sid)
                .ok()
                .flatten()
                .is_some_and(|c| c.state == "recovered")
    });
    // Give a replay, if there were one, the time to happen.
    std::thread::sleep(Duration::from_secs(3));
    let history = store(&home).load_history(&sid).unwrap();
    assert_eq!(user_rows(&home, &sid, A), 1, "A ran once");
    assert_eq!(
        user_rows(&home, &sid, B),
        1,
        "B's prompt reached the transcript once — its queue row was not replayed\n{}",
        log_tail(&gw2.log)
    );
    assert_eq!(
        store(&home).turn_checkpoint(&sid).unwrap().map(|c| c.state),
        Some("recovered".to_string()),
        "the interrupted turn was continued and marked recovered"
    );
    assert!(
        history
            .iter()
            .any(|m| m.injection.as_deref() == Some("restart")),
        "the continuation says the gateway restarted"
    );
    assert!(
        history
            .iter()
            .filter(|m| m.role == "assistant" && m.content == REPLY)
            .count()
            >= 2,
        "A's answer and B's continued answer"
    );
    assert!(
        store(&home).queue_items(&sid).unwrap().is_empty(),
        "nothing left queued"
    );
    drop(gw2);
    let _ = std::fs::remove_dir_all(&home);
}
