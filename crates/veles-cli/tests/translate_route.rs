//! Selection translation through a REAL gateway (TR-1, TR-3): the
//! translator is a provider OUTSIDE the chat chain, and with no
//! `[translation]` section the provider that lists the default translator
//! model (`deepseek-v4.1-flash-1017`) is used as it stands. Before the
//! fix `/api/translate` answered "translator provider is not configured"
//! for exactly this shape, the owner's: the route was looked up only in
//! the `[model]` primary and `[[fallback_providers]]`.
//!
//! Hermetic: a scratch VELES_HOME under the per-run TMPDIR, its own
//! loopback port, two stub providers in this process, no cluster, a2a or
//! connector configuration.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TOKEN: &str = "tok-translate-route-test";

/// A non-streaming chat.completions stub that records each request body
/// and answers `reply`.
fn stub_provider(reply: &'static str) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a stub provider");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let log = log.clone();
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
                log.lock().unwrap().push(req);
                let body = json!({
                    "id": "stub",
                    "object": "chat.completion",
                    "choices": [{"index": 0, "finish_reason": "stop",
                        "message": {"role": "assistant", "content": reply}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
                let _ = s.flush();
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
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
    let mut s = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(5),
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
fn a_translator_outside_the_chat_chain_translates_with_no_translation_section() {
    let (chat_url, chat_seen) = stub_provider("chat answer");
    let (tr_url, tr_seen) = stub_provider("Hallo, {name}! 50%");
    let home: PathBuf = std::env::temp_dir().join(format!(
        "veles-translate-route-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".env"), format!("DESKTOP_BEARER_TOKEN={TOKEN}\n")).unwrap();
    // The owner's shape: the chat chain is another provider, the
    // translator's provider is in neither [model] nor a fallback, and
    // there is no [translation] section at all.
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[model]\nprovider = \"chat\"\ndefault = \"chat-model\"\n\n\
             [[custom_providers]]\nname = \"chat\"\nbase_url = \"{chat_url}\"\n\
             api_mode = \"chat_completions\"\nmodel = \"chat-model\"\n\n\
             [[custom_providers]]\nname = \"ali-token-plan-personal\"\nbase_url = \"{tr_url}\"\n\
             api_mode = \"chat_completions\"\n\
             models = [\"qwen3.8-flash\", \"deepseek-v4.1-flash-1017\"]\n"
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
            Duration::from_secs(5),
        )
        .is_ok()
            && http(port, "GET", "/api/health", "").0 == 200
    });

    let (status, body) = http(port, "GET", "/api/translate/config", "");
    let cfg: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    assert_eq!(status, 200, "{body}");
    assert_eq!(cfg["configured"], json!(true), "readiness: {body}");

    let request = json!({"text": "Hello, {name}! 50%", "target_language": "German"}).to_string();
    let (status, body) = http(port, "POST", "/api/translate", &request);
    let answer: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    assert_eq!(
        status,
        200,
        "translate: {body}\n--- gateway log ---\n{}\n{}",
        log_tail(&log),
        log_tail(&home.join("gw.err"))
    );
    assert_eq!(answer["text"], json!("Hallo, {name}! 50%"));
    assert_eq!(answer["provider"], json!("ali-token-plan-personal"));
    assert_eq!(answer["model"], json!("deepseek-v4.1-flash-1017"));

    let seen = tr_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one call to the translator: {seen:?}");
    let call = &seen[0];
    assert_eq!(call["model"], json!("deepseek-v4.1-flash-1017"));
    assert!(
        call["tools"].as_array().is_none_or(|t| t.is_empty()),
        "{call}"
    );
    assert_ne!(call["stream"], json!(true), "{call}");
    let messages = call["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(messages.len(), 2, "system + selection only: {call}");
    assert!(messages[0]["content"]
        .as_str()
        .unwrap_or("")
        .contains("German"));
    assert_eq!(messages[1]["content"], json!("Hello, {name}! 50%"));
    assert!(
        chat_seen.lock().unwrap().is_empty(),
        "the chat route must not serve a translation"
    );
    let _ = std::fs::remove_dir_all(&home);
}
