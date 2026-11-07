//! The gateway stays answerable while a long turn ends (review DR-3,
//! the gateway half of the owner's end-of-turn stall report: "doesn't
//! block seemingly everything else on the gateway").
//!
//! A REAL gateway and a scripted provider. One turn makes many tool
//! rounds with identical arguments, so every round also fires a repeat
//! guard note and the session ends up with a large history and a long
//! note list — the shape that stalled. While it runs and while it ends,
//! three pollers ask what a desktop asks (`/api/status`, the session's
//! `/context`, the session list); a watcher on the session's event
//! stream stamps the `turn-result`; the moment it arrives the test
//! fetches `/history` as the desktop's reconcile does. The worst answer
//! in the window around the turn end must stay well under a second.
//!
//! The bound is generous on purpose (a loaded CI box is slow, not
//! stalled): a stall is the whole gateway waiting on one turn's
//! bookkeeping, which shows as a multi-second gap. The measured figures
//! are printed (`--nocapture`) so a run doubles as the measurement.
//! Mid-turn answers are printed, not asserted: on a loaded box they
//! include host scheduling pauses no gateway change removes. The
//! mid-turn cause this measurement did find, `/context` waiting for the
//! store lock behind the turn's writes, is pinned deterministically by
//! desktop_api's `the_context_figure_reads_beside_a_held_store_lock`.
//! A turn-end stall injected for the check (a 2 s store-lock hold in
//! `publish_verdict`) turns this test red.
//!
//! Hermetic: a scratch VELES_HOME under the per-run TMPDIR, its own
//! port, the provider in this process.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TOKEN: &str = "tok-turn-end-latency";
/// Tool rounds in the long turn.
const ROUNDS: usize = 60;

fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// The scripted model: a user message carrying LONGTURN gets
/// `read_file` on the same file until ROUNDS tool results are in the
/// request, then a final answer. Anything else (titles) gets "ok".
fn stub_provider(file: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    static CALLS: AtomicU64 = AtomicU64::new(1);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let file = file.clone();
            std::thread::spawn(move || {
                let mut raw = Vec::new();
                let mut buf = [0u8; 65536];
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
                let msgs = req["messages"].as_array().cloned().unwrap_or_default();
                // This turn's rounds: tool results after the last user line.
                let last_user = msgs.iter().rposition(|m| m["role"] == "user");
                let user_text = last_user
                    .and_then(|i| msgs[i]["content"].as_str().map(str::to_string))
                    .unwrap_or_default();
                let rounds = last_user
                    .map(|i| msgs[i..].iter().filter(|m| m["role"] == "tool").count())
                    .unwrap_or(0);
                let has_tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
                // A provider is not instant; neither is this one.
                std::thread::sleep(Duration::from_millis(15));
                let body = if has_tools && user_text.contains("LONGTURN") && rounds < ROUNDS {
                    let n = CALLS.fetch_add(1, Ordering::SeqCst);
                    let args = json!({ "path": file }).to_string();
                    sse(&[
                        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":format!("round {rounds}. "),"tool_calls":[
                            {"index":0,"id":format!("call_rf_{n}"),"type":"function",
                             "function":{"name":"read_file","arguments":args}}]}}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                        json!({"choices":[],"usage":{"prompt_tokens":1000 + rounds * 100,"completion_tokens":10,"total_tokens":1010 + rounds * 100}}),
                    ])
                } else if user_text.contains("LONGTURN") {
                    sse(&[
                        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"LONGTURN DONE"}}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    ])
                } else {
                    sse(&[
                        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"ok"}}]}),
                    ])
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
    let Ok(mut s) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(5),
    ) else {
        return (0, String::new());
    };
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    if s.write_all(req.as_bytes()).is_err() {
        return (0, String::new());
    }
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
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// (started, latency) samples of one poller.
type Samples = Arc<Mutex<Vec<(Instant, Duration)>>>;

fn poller(port: u16, path: String, every: Duration, stop: Arc<AtomicBool>) -> Samples {
    let out: Samples = Arc::new(Mutex::new(Vec::new()));
    let o = out.clone();
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let t = Instant::now();
            let (code, _) = http(port, "GET", &path, "");
            let took = t.elapsed();
            if code == 200 {
                o.lock().unwrap().push((t, took));
            }
            std::thread::sleep(every);
        }
    });
    out
}

/// The worst latency of a poll that was in flight anywhere in
/// [from, to], and how many such polls there were.
fn worst(s: &Samples, from: Instant, to: Instant) -> (Duration, usize) {
    let v = s.lock().unwrap();
    let hits: Vec<Duration> = v
        .iter()
        .filter(|(t, d)| *t <= to && *t + *d >= from)
        .map(|(_, d)| *d)
        .collect();
    (hits.iter().copied().max().unwrap_or_default(), hits.len())
}

#[test]
fn the_gateway_answers_while_a_long_many_guard_turn_ends() {
    let home: PathBuf = std::env::temp_dir().join(format!(
        "veles-turn-end-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    // ~24 KB: every round adds a real tool result to the history.
    let file = home.join("big.txt");
    let line = "the quick brown fox jumps over the lazy dog 0123456789\n";
    std::fs::write(&file, line.repeat(24_000 / line.len())).unwrap();
    let url = stub_provider(file.display().to_string());
    std::fs::write(home.join(".env"), format!("DESKTOP_BEARER_TOKEN={TOKEN}\n")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{url}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n\n\
             # every identical repeat warns; none halts\n\
             [agent.guards]\nguardrail_warn_at = 2\nguardrail_halt_at = 100000\n"
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
        http(port, "GET", "/api/health", "").0 == 200
    });
    let (code, body) = http(port, "POST", "/api/sessions/new", r#"{"title":"long"}"#);
    assert_eq!(code, 201, "sessions/new: {body}");
    let sid = serde_json::from_str::<Value>(&body).unwrap()["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    // The session's event stream, as a desktop window holds it.
    let turn_result: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let notes = Arc::new(AtomicU64::new(0));
    {
        let (tr, notes, sid) = (turn_result.clone(), notes.clone(), sid.clone());
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("events");
        write!(
            s,
            "GET /api/sessions/{sid}/events HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Authorization: Bearer {TOKEN}\r\nAccept: text/event-stream\r\n\r\n"
        )
        .unwrap();
        std::thread::spawn(move || {
            for l in BufReader::new(s).lines() {
                let Ok(l) = l else { break };
                if l.starts_with("event: turn-result") {
                    tr.lock().unwrap().get_or_insert(Instant::now());
                } else if l.starts_with("event: note") {
                    notes.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
    }

    let stop = Arc::new(AtomicBool::new(false));
    let status = poller(
        port,
        "/api/status".into(),
        Duration::from_millis(50),
        stop.clone(),
    );
    let context = poller(
        port,
        format!("/api/sessions/{sid}/context"),
        Duration::from_millis(100),
        stop.clone(),
    );
    let sessions = poller(
        port,
        "/api/sessions".into(),
        Duration::from_millis(250),
        stop.clone(),
    );

    let started = Instant::now();
    let (code, body) = http(
        port,
        "POST",
        "/api/chat",
        &json!({"session_id": sid, "text": "LONGTURN read it again and again"}).to_string(),
    );
    assert!(code == 200 || code == 202, "chat: {code} {body}");
    wait_until("the long turn's result", 300, || {
        turn_result.lock().unwrap().is_some()
    });
    let end = turn_result.lock().unwrap().unwrap();
    // What the desktop's reconcile does on turn-result.
    let t = Instant::now();
    let (hcode, hist) = http(port, "GET", &format!("/api/sessions/{sid}/history"), "");
    let history_took = t.elapsed();
    std::thread::sleep(Duration::from_secs(3));
    stop.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(hcode, 200);
    let hv: Value = serde_json::from_str(&hist).unwrap_or(Value::Null);
    let rows = hv["messages"].as_array().map(Vec::len).unwrap_or(0);
    let from = end - Duration::from_secs(2);
    let to = end + Duration::from_secs(3);
    let (st_end, st_n) = worst(&status, from, to);
    let (cx_end, cx_n) = worst(&context, from, to);
    let (ss_end, ss_n) = worst(&sessions, from, to);
    let (st_mid, _) = worst(&status, started, from);
    let (cx_mid, _) = worst(&context, started, from);
    eprintln!(
        "turn: {:?} for {ROUNDS} rounds, {} note events live, history {} rows / {} KB in {:?}\n\
         worst around turn end (-2s..+3s): /api/status {:?} ({st_n} polls), \
         /context {:?} ({cx_n}), /api/sessions {:?} ({ss_n})\n\
         worst mid-turn: /api/status {:?}, /context {:?}",
        end - started,
        notes.load(Ordering::SeqCst),
        rows,
        hist.len() / 1024,
        history_took,
        st_end,
        cx_end,
        ss_end,
        st_mid,
        cx_mid
    );
    // Every slow answer, by when it was asked relative to the POST and
    // to the turn end, so a stall can be placed.
    for (what, s) in [
        ("/api/status", &status),
        ("/context", &context),
        ("/api/sessions", &sessions),
    ] {
        for (t, d) in s
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, d)| *d > Duration::from_millis(250))
        {
            eprintln!(
                "slow {what}: {d:?} asked at +{:?} (turn end at +{:?})",
                t.duration_since(started),
                end - started
            );
        }
    }
    assert!(
        hist.contains("LONGTURN DONE"),
        "the turn finished with its answer"
    );
    assert!(
        notes.load(Ordering::SeqCst) as usize >= ROUNDS / 2,
        "the turn fired its guards"
    );
    assert!(
        st_n >= 10 && cx_n >= 5,
        "the pollers ran across the turn end"
    );
    let bound = Duration::from_millis(1000);
    for (what, d) in [
        ("/api/status", st_end),
        ("/context", cx_end),
        ("/api/sessions", ss_end),
        ("/history", history_took),
    ] {
        assert!(d < bound, "{what} took {d:?} around the turn end");
    }
    let _ = std::fs::remove_dir_all(&home);
}
