//! An app asking the agent, end to end (PLAN-sessions-and-phone.md A4,
//! USER decision D6): a REAL gateway, an app whose app.toml declares
//! `ask`, and its question through `/api/apps/:name/ask` — the route the
//! window's `veles::ask` takes. The turn runs in the app's own session as
//! `app:<name>`, a User: when the model reaches for an owner-only tool
//! from there, it is refused. An app that did not declare `ask` cannot.
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
                } else if has_tools && text.contains("APP-ASK-CONFIG") {
                    call(json!({"action":"get","path":"model.default"}))
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

#[test]
fn an_app_asks_as_itself_and_only_if_it_declared_ask() {
    let url = stub_provider();
    let home: PathBuf = std::env::temp_dir().join(format!(
        "veles-app-ask-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(home.join("apps/eggs")).unwrap();
    std::fs::create_dir_all(home.join("apps/plain")).unwrap();
    std::fs::write(home.join(".env"), format!("DESKTOP_BEARER_TOKEN={TOKEN}\n")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{url}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        home.join("apps/eggs/app.toml"),
        "title = \"Eggs\"\ncapabilities = [\"ask\"]\n",
    )
    .unwrap();
    std::fs::write(
        home.join("apps/eggs/main.tcl"),
        "pack [ttk::label .l -text eggs]\n",
    )
    .unwrap();
    std::fs::write(home.join("apps/plain/app.toml"), "title = \"Plain\"\n").unwrap();
    std::fs::write(
        home.join("apps/plain/main.tcl"),
        "pack [ttk::label .l -text p]\n",
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

    let ask = |app: &str, text: &str| -> (u16, Value) {
        let (code, body) = http(
            port,
            "POST",
            &format!("/api/apps/{app}/ask"),
            &json!({"text": text}).to_string(),
        );
        (code, serde_json::from_str(&body).unwrap_or(Value::Null))
    };

    let (code, r) = ask("eggs", "how many eggs?");
    assert_eq!(code, 200, "{r}\n{}", log_tail(&log));
    assert_eq!(r["reply"], "ok", "{r}");
    let sid = r["session_id"]
        .as_str()
        .expect("the app's session")
        .to_string();

    // The app's own session, kept and reused.
    let (_, r2) = ask("eggs", "and now?");
    assert_eq!(r2["session_id"], json!(sid), "{r2}");
    let (_, list) = http(port, "GET", "/api/sessions", "");
    assert!(list.contains("App: Eggs"), "{list}");

    // From there an owner-only tool is refused: the app asks as itself.
    let (_, r) = ask("eggs", "APP-ASK-CONFIG what model is this?");
    let reply = r["reply"].as_str().unwrap_or("");
    assert!(
        reply.contains("TOOL SAID")
            && reply.contains("access denied")
            && reply.contains("owner-only"),
        "{r}"
    );

    // An app that did not declare `ask` is refused at the door.
    let (code, r) = ask("plain", "hello");
    assert_eq!(code, 403, "{r}");
    let (code, _) = ask("nosuch", "hello");
    assert_eq!(code, 404);
    let _ = std::fs::remove_dir_all(&home);
}
