//! A delegate child's background shells end with it: when its turn
//! ends, and when the parent asks it to stop (SIGTERM, what
//! `delegate_task` sends at a cancel or at a time limit someone set).
//!
//! WHY IT EXISTS. On 2026-11-17 every child of a goonix session was
//! killed at the old fixed 900 s, and a test daemon one of them had
//! started in a background shell was still running 2.5 hours later: the
//! shell's tree is a process group of its own, so killing the child
//! orphaned it, and a child whose turn simply ended left its shells
//! running too, in a registry nobody could read once it exited.
//!
//! The real binary, the real argv builder, the real turn loop and the
//! real `terminal` tool, against a scripted provider in this process:
//! round one starts `sleep 300` in a background shell and records its
//! pid; round two either ends the turn or runs a foreground `sleep 60`
//! that the stop has to interrupt. Hermetic: a scratch VELES_HOME, no
//! network, no key.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn tool_call(id: &str, args: Value) -> String {
    sse(&[
        json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
            {"index":0,"id":id,"type":"function",
             "function":{"name":"terminal","arguments":args.to_string()}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ])
}

fn answer(text: &str) -> String {
    sse(&[
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ])
}

/// The scripted model. `second` is round two's answer. Returns the URL
/// and the count of turn requests served (title calls, without tools,
/// are not counted).
fn stub_provider(pidfile: PathBuf, second: fn() -> String) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    let served = Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let (pidfile, counter) = (pidfile.clone(), counter.clone());
            std::thread::spawn(move || {
                // The whole request before the answer (pitfall 87).
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
                let last_user = msgs.iter().rposition(|m| m["role"] == "user");
                let rounds = last_user
                    .map(|i| msgs[i..].iter().filter(|m| m["role"] == "tool").count())
                    .unwrap_or(0);
                let has_tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
                let body = if !has_tools {
                    answer("ok")
                } else {
                    counter.fetch_add(1, Ordering::SeqCst);
                    match rounds {
                        0 => tool_call(
                            "call_bg",
                            json!({
                                "command": format!(
                                    "sleep 300 </dev/null >/dev/null 2>&1 & echo $! > {}; wait",
                                    pidfile.display()
                                ),
                                "background": true,
                            }),
                        ),
                        1 => second(),
                        _ => answer("DONE"),
                    }
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
    (format!("http://{addr}/v1"), served)
}

fn scratch_home(url: &str, tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-child-shells-{tag}-{}-{:x}",
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
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{url}\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n\n\
             # the commands are this test's own; no screen, no question\n\
             [approvals]\nmode = \"off\"\n\n\
             # round two's foreground sleep is the point, not a habit to refuse\n\
             [agent.guards]\nforeground_sleep_max_secs = 0\n"
        ),
    )
    .unwrap();
    d
}

/// Spawn the child exactly as `delegate_task` does, into a pre-minted
/// child session.
fn spawn_child(home: &Path, query: &str) -> std::process::Child {
    let child = "20260101_000000_shellchild";
    {
        let store = veles_core::store::Store::open(&home.join("state.db")).expect("store");
        store.create_session("shells-parent").expect("parent");
        store
            .create_child_session(child, "shells-parent", Some("subagent: shells"))
            .expect("child row");
    }
    let argv = veles_core::delegate::child_argv(query, Some(child), None, None, None);
    std::process::Command::new(env!("CARGO_BIN_EXE_veles"))
        .args(&argv)
        .env("VELES_HOME", home)
        .env("VELES_CHILD_DEPTH", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the real binary")
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_exit(c: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Ok(Some(s)) = c.try_wait() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn read_pid(f: &Path) -> Option<u32> {
    std::fs::read_to_string(f).ok()?.trim().parse().ok()
}

fn gone_soon(pid: u32) -> bool {
    (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        !alive(pid)
    })
}

fn stderr_of(c: &mut std::process::Child) -> String {
    let mut s = String::new();
    if let Some(mut e) = c.stderr.take() {
        let _ = e.read_to_string(&mut s);
    }
    s
}

#[test]
fn a_childs_background_shell_ends_with_its_turn() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("bg.pid");
    let (url, _) = stub_provider(pidfile.clone(), || answer("DONE"));
    let home = scratch_home(&url, "end");
    let mut c = spawn_child(&home, "start the background job, then finish");
    let status = wait_exit(&mut c, 120);
    let err = stderr_of(&mut c);
    assert!(status.is_some(), "the child never finished: {err}");
    let pid = read_pid(&pidfile).unwrap_or_else(|| panic!("no background shell ran: {err}"));
    let gone = gone_soon(pid);
    if !gone {
        // SAFETY: the test's own stray sleep.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    assert!(
        gone,
        "the background shell's sleep {pid} outlived the child's turn: {err}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_child_asked_to_stop_ends_its_turn_and_its_shells() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("bg.pid");
    let (url, served) = stub_provider(pidfile.clone(), || {
        tool_call("call_fg", json!({"command": "sleep 60", "timeout": 120}))
    });
    let home = scratch_home(&url, "stop");
    let mut c = spawn_child(
        &home,
        "start the background job, then wait in the foreground",
    );
    // Round two (the foreground sleep) has been asked for, and the
    // background shell has started.
    let deadline = Instant::now() + Duration::from_secs(120);
    while (served.load(Ordering::SeqCst) < 2 || read_pid(&pidfile).is_none())
        && Instant::now() < deadline
    {
        assert!(
            c.try_wait().unwrap().is_none(),
            "the child ended early: {}",
            stderr_of(&mut c)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = read_pid(&pidfile).expect("no background shell ran");
    std::thread::sleep(Duration::from_millis(1000));
    // Still in round two's sleep: a child that had already finished (its
    // sleep refused by a guard, as the first version of this test let
    // happen) would pass everything below without ever being stopped.
    assert!(
        c.try_wait().unwrap().is_none(),
        "the child finished before it was asked to stop: {}",
        stderr_of(&mut c)
    );
    let t0 = Instant::now();
    // SAFETY: the child this test spawned.
    unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
    let status = wait_exit(&mut c, 30);
    let err = stderr_of(&mut c);
    if status.is_none() {
        let _ = c.kill();
    }
    assert!(status.is_some(), "the child did not stop when asked: {err}");
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "the stop took {:?}: {err}",
        t0.elapsed()
    );
    let gone = gone_soon(pid);
    if !gone {
        // SAFETY: the test's own stray sleep.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    assert!(
        gone,
        "the background shell's sleep {pid} outlived the stop: {err}"
    );
    // The stop ended the turn where it stood: the model was never asked
    // for a third round.
    assert_eq!(
        served.load(Ordering::SeqCst),
        2,
        "the turn went on after the stop: {err}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
