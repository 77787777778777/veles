//! A REAL child process, streaming its turn to a REAL socket.
//!
//! WHY IT EXISTS. `session_tap` is one process-wide broadcast channel
//! and a delegate child is a separate `veles chat -q` process, so a
//! child published a whole turn — deltas, reasoning, tool rounds — to
//! a bus with no subscribers while every attached window listened in
//! the gateway. Attach to a running child in the desktop and you saw
//! the store replay, then silence. USER 2026-09-18: "I could see its
//! progress but then at some point it stopped streaming and looked
//! hung even though there were new messages coming in."
//!
//! The gateway half of the relay is unit-tested in `desktop_api.rs`
//! (`relay_ingest` republishes, restamps `seq`, routes the turn
//! lifecycle, sits behind the bearer). This is the half those cannot
//! reach: that a child process, spawned the way `delegate_task` spawns
//! one, running the real binary and the real turn loop, actually puts
//! its events on the wire — in order, before it exits.
//!
//! The lesson this file is built on (HANDOFF pitfall 69): a test that
//! points at a fake proves the caller, never the callee. So the child
//! here is the real binary and the socket is a real socket; only the
//! provider and the gateway are stubs, and both are stubs that record.
//!
//! HERMETIC AND FREE. No network, no key, no tokens.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One canned chat.completions stream, served to the first caller.
fn stub_provider(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let Ok(mut s) = stream else { continue };
            // Read the WHOLE request (headers, then Content-Length bytes)
            // before answering: a close with request bytes still unread
            // is a RST, and on Windows the client then reports "error
            // decoding response body" instead of the stream it was sent
            // (one `read` of the headers was enough on Linux by timing).
            let mut req = Vec::new();
            let mut buf = [0u8; 8192];
            let mut want = None;
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
                if want.is_none() {
                    if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        want = Some(end + 4 + len);
                    }
                }
                if want.is_some_and(|w| req.len() >= w) {
                    break;
                }
            }
            let body = format!(
                concat!(
                    "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\"}}}}]}}\n\n",
                    "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"weighing it\"}}}}]}}\n\n",
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{reply}\"}}}}]}}\n\n",
                    "data: [DONE]\n\n",
                ),
                reply = reply
            );
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
            let _ = s.flush();
        }
    });
    format!("http://{addr}")
}

/// A gateway that does nothing but record the relay bodies it is sent.
///
/// Returns its base URL and the shared buffer of request bodies. Every
/// POST is answered 204, the same as the real `relay_ingest`.
fn recording_gateway() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub gateway");
    let addr = listener.local_addr().expect("addr");
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            // Read head, then exactly Content-Length bytes of body —
            // a short read here would truncate a batch and the test
            // would be asserting on half a transcript.
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let mut head_end = None;
            while head_end.is_none() {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                    }
                    Err(_) => break,
                }
            }
            let Some(he) = head_end else { continue };
            let head = String::from_utf8_lossy(&raw[..he]).to_string();
            let len: usize = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut body = raw[he..].to_vec();
            while body.len() < len {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => body.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            if head.contains("/api/session-events") {
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).to_string());
            }
            let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            let _ = s.flush();
        }
    });
    (format!("http://{addr}"), seen)
}

fn scratch_home(base_url: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-child-relay-{}-{:x}",
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
             api_mode = \"chat_completions\"\nmodel = \"stub-model\"\n"
        ),
    )
    .unwrap();
    d
}

#[test]
fn a_child_puts_its_turn_on_the_wire_before_it_exits() {
    let url = stub_provider("the relayed answer");
    let (gateway, seen) = recording_gateway();
    let home = scratch_home(&url);

    let child = "20260101_000000_relayee";
    {
        let store = veles_core::store::Store::open(&home.join("state.db")).expect("store");
        store.create_session("relay-parent").expect("parent");
        store
            .create_child_session(child, "relay-parent", Some("subagent: relay me"))
            .expect("child row");
    }

    let argv = veles_core::delegate::child_argv("relay me", Some(child), None, None, None);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_veles"))
        .args(&argv)
        .env("VELES_HOME", &home)
        .env("VELES_CHILD_DEPTH", "1")
        .env("VELES_RELAY_TO", &gateway)
        .output()
        .expect("spawn the real binary");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the child exited {:?}\n{stderr}",
        out.status.code()
    );

    // The child exits the moment the turn does. Mutation-checked, and
    // the result was stronger than expected: dropping the handle
    // instead of awaiting `flush` posts NOTHING AT ALL — in a process
    // this short-lived the forwarder task never gets scheduled. The
    // flush is not an optimisation for the last few events, it is the
    // whole relay.
    let bodies = seen.lock().unwrap().clone();
    let all = bodies.join("\n");
    assert!(
        !bodies.is_empty(),
        "the child posted nothing at all — its events went to a bus \
         with no subscribers, which is the whole bug: {stderr}"
    );
    assert!(
        all.contains("turn-started"),
        "a window attaching mid-turn needs the lifecycle, not just \
         tokens: {all}"
    );
    assert!(
        all.contains("the relayed answer"),
        "the answer the child streamed has to reach the wire: {all}"
    );
    assert!(
        all.contains("weighing it"),
        "and so does the reasoning — row-granularity following already \
         carried the answer; live reasoning is what the relay adds: {all}"
    );
    assert!(
        all.contains("turn-ended"),
        "…and the end, or the gateway keeps a snapshot for a turn that \
         finished and the staleness guard has to clean up after it: {all}"
    );
    // Ordering is the contract: a transcript is an ordering, and two
    // concurrent posts would break it.
    let start = all.find("turn-started").unwrap();
    let answer = all.find("the relayed answer").unwrap();
    let end = all.find("turn-ended").unwrap();
    assert!(
        start < answer && answer < end,
        "events must arrive in the order they happened: \
         start={start} answer={answer} end={end}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
