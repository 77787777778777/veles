//! The `config` tool end to end (PLAN-sessions-and-phone.md, round P2):
//! a REAL gateway with a token-protected desktop API, a model that calls
//! the tool, and the owner's answer given through the API's own approval
//! route — the path a phone's chat takes when the person says "make it
//! compact sooner".
//!
//! The tool runs inside the gateway and writes through the gateway's own
//! `/api/config/set` (with the token), so the change is reloaded into the
//! live handle, not only written to disk.
//!
//! Hermetic: a scratch VELES_HOME under the per-run TMPDIR, its own port,
//! a stub provider in this process.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TOKEN: &str = "tok-config-tool-test";
const KEY: &str = "agent.compaction.trigger_percent";

/// One chat.completions answer as an SSE body.
fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// The scripted model. A user message carrying a marker gets a
/// `tool_call` envelope (`config` is deferred, so that is how a model
/// reaches it); a tool result is said back as "TOOL SAID: …" so the test
/// reads the tool's own words from the transcript; anything else (titles)
/// gets "ok".
fn stub_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    static CALLS: AtomicU64 = AtomicU64::new(1);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut raw = Vec::new();
                let mut buf = [0u8; 16384];
                let mut need: Option<usize> = None;
                let mut body_at = 0;
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
                            body_at = end + 4;
                            need = Some(end + 4 + len);
                        }
                    }
                    if matches!(need, Some(t) if raw.len() >= t) {
                        break;
                    }
                }
                let req: Value =
                    serde_json::from_slice(&raw[body_at.min(raw.len())..]).unwrap_or(Value::Null);
                let last = req["messages"]
                    .as_array()
                    .and_then(|m| m.last())
                    .cloned()
                    .unwrap_or(Value::Null);
                let text = match &last["content"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let has_tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
                let call = |args: Value| {
                    let n = CALLS.fetch_add(1, Ordering::SeqCst);
                    let envelope = json!({"name": "config", "arguments": args}).to_string();
                    sse(&[
                        json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                            {"index":0,"id":format!("call_cfg_{n}"),"type":"function",
                             "function":{"name":"tool_call","arguments":envelope}}]}}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                    ])
                };
                let say = |t: String| {
                    sse(&[
                        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":t}}]}),
                    ])
                };
                let body = if last["role"] == "tool" {
                    say(format!("TOOL SAID: {text}"))
                } else if has_tools && text.contains("CONFIG-E2E-ACL") {
                    call(json!({"action":"set","path":"acl.lockdown","value":false}))
                } else if has_tools && text.contains("CONFIG-E2E") {
                    call(json!({"action":"set","path":KEY,"value":61}))
                } else {
                    say("ok".into())
                };
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

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    // `connect_timeout`, not `connect`: on Windows a loopback SYN can sit
    // unanswered for minutes (PLAN-windows.md §3 notes, 2026-10-13); a bounded
    // wait keeps the retry loop alive instead of hanging the test.
    let mut s = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(5),
    )
    .expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
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

fn log_tail(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// The open asks of one session.
fn asks_for(port: u16, sid: &str) -> Vec<Value> {
    let (_, body) = http(port, "GET", "/api/asks", "");
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    v["asks"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a["session_id"] == sid)
        .collect()
}

/// Every assistant line in the session's transcript, joined.
fn said(port: u16, sid: &str) -> String {
    let (_, body) = http(port, "GET", &format!("/api/sessions/{sid}/history"), "");
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let msgs = v["messages"]
        .as_array()
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default();
    msgs.iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n")
}

fn chat(port: u16, sid: &str, text: &str) {
    let (code, body) = http(
        port,
        "POST",
        "/api/chat",
        &json!({"session_id": sid, "text": text}).to_string(),
    );
    assert!(code == 200 || code == 202, "chat: {code} {body}");
}

#[test]
fn the_owner_is_asked_and_only_a_yes_changes_the_live_config() {
    let url = stub_provider();
    let home: PathBuf = std::env::temp_dir().join(format!(
        "veles-config-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".env"), format!("DESKTOP_BEARER_TOKEN={TOKEN}\n")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{url}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n\n\
             # the owner's own note\n[agent.compaction]\ntrigger_percent = 70\n"
        ),
    )
    .unwrap();
    let port = free_port();
    let log = home.join("gw.log");
    let _gw = Proc(
        Command::new(env!("CARGO_BIN_EXE_veles"))
            .args(["gateway", "run"])
            .env("VELES_HOME", &home)
            .env("HOME", &home)
            .env("VELES_DESKTOP_PORT", port.to_string())
            .env_remove("VELES_DESKTOP_OPEN")
            .env_remove("DESKTOP_BEARER_TOKEN")
            .env_remove("VELES_SESSION_PRINCIPAL")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(std::fs::File::create(home.join("gw.err")).unwrap())
            .spawn()
            .expect("spawn the gateway"),
    );
    wait_until("the gateway's health route", 60, || {
        TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            std::time::Duration::from_secs(5),
        )
        .is_ok()
            && http(port, "GET", "/api/health", "").0 == 200
    });
    let (code, body) = http(port, "POST", "/api/sessions/new", r#"{"title":"settings"}"#);
    assert_eq!(code, 201, "sessions/new: {body}");
    let sid = serde_json::from_str::<Value>(&body).unwrap()["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    let config = || std::fs::read_to_string(home.join("config.toml")).unwrap();

    // 1. Asked, and the owner says no: nothing is written.
    chat(port, &sid, "CONFIG-E2E compact sooner please");
    let mut ask = Value::Null;
    wait_until("the approval card", 60, || {
        ask = asks_for(port, &sid)
            .into_iter()
            .next()
            .unwrap_or(Value::Null);
        !ask.is_null()
    });
    assert!(ask["prompt"].as_str().unwrap_or("").contains(KEY), "{ask}");
    let detail = ask["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("now: 70") && detail.contains("new: 61"),
        "{ask}"
    );
    let id = ask["id"].as_str().unwrap().to_string();
    let (_, r) = http(
        port,
        "POST",
        &format!("/api/approvals/{id}/decide"),
        r#"{"choice":"deny"}"#,
    );
    assert!(r.contains("true"), "decide: {r}");
    wait_until("the refused turn's reply", 60, || {
        said(port, &sid).contains("TOOL SAID")
    });
    assert!(said(port, &sid).contains("said no"), "{}", said(port, &sid));
    assert!(config().contains("trigger_percent = 70"), "{}", config());

    // 2. Asked again, and the owner says yes: written through the
    // gateway (comments kept) and live in its handle.
    chat(port, &sid, "CONFIG-E2E go on then");
    wait_until("the second approval card", 60, || {
        !asks_for(port, &sid).is_empty()
    });
    let id = asks_for(port, &sid)[0]["id"].as_str().unwrap().to_string();
    http(
        port,
        "POST",
        &format!("/api/approvals/{id}/decide"),
        r#"{"choice":"once"}"#,
    );
    wait_until("the approved turn's reply", 60, || {
        said(port, &sid).contains("is now 61")
    });
    let text = config();
    assert!(text.contains("trigger_percent = 61"), "{text}");
    assert!(text.contains("# the owner's own note"), "{text}");
    assert!(
        !said(port, &sid).contains("no gateway is running"),
        "the write went through the gateway: {}\n{}",
        said(port, &sid),
        log_tail(&log)
    );
    let (_, got) = http(port, "GET", &format!("/api/config/get?path={KEY}"), "");
    assert!(got.contains("61"), "{got}");

    // 3. Who may reach the agent is refused before anyone is asked: the
    // turn ends with no card to answer.
    chat(port, &sid, "CONFIG-E2E-ACL open up");
    wait_until("the refusal", 60, || {
        said(port, &sid).contains("does not change it")
    });
    assert!(asks_for(port, &sid).is_empty());
    assert!(!config().contains("lockdown"), "{}", config());
    let _ = std::fs::remove_dir_all(&home);
}
