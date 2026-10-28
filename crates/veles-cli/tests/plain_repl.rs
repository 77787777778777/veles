//! The plain REPL (`veles chat --plain`) prints a reply ONCE.
//!
//! It streamed the reply's text as it arrived and then, when the turn
//! ended, printed the whole reply again through the markdown renderer:
//! every answer appeared twice ("pong\npong"), which a screen reader (the
//! surface's stated audience) reads out twice too.
//!
//! Hermetic, as a_child_actually_answers.rs is: the provider is a
//! listener in this process serving one OpenAI-shaped SSE completion,
//! named by a scratch config.toml under a scratch VELES_HOME.
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// One canned streamed completion for the first caller. The whole request
/// is read before answering (pitfall 87: an unread body turns the close
/// into an RST that discards the reply).
fn stub_provider(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let Ok(mut s) = stream else { continue };
            let mut req = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                req.extend_from_slice(&chunk[..n]);
                if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&req[..end]).to_string();
                    let len: usize = head
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse().ok())
                        .unwrap_or(0);
                    while req.len() < end + 4 + len {
                        match s.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&chunk[..n]),
                        }
                    }
                    break;
                }
            }
            let body = format!(
                concat!(
                    "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\"}}}}]}}\n\n",
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

#[test]
fn the_plain_repl_prints_a_streamed_reply_once() {
    let url = stub_provider("unmistakable-reply-7f3a");
    let home = tempfile::tempdir().expect("scratch home");
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"{url}\"\n\
             api_mode = \"chat_completions\"\nmodel = \"stub-model\"\n"
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_veles"))
        .args(["chat", "--plain"])
        .env("VELES_HOME", home.path())
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the real binary");
    // One prompt, then EOF: the REPL answers it and says goodbye.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"say the word\n")
        .unwrap();
    let out = child.wait_with_output().expect("the REPL exits on EOF");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let seen = stdout.matches("unmistakable-reply-7f3a").count();
    assert_eq!(
        seen, 1,
        "the reply should be printed once, was {seen} times\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}
