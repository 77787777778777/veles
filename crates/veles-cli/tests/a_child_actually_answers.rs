//! THE AUDIT'S LOAD-BEARING TEST: a delegate child, spawned the way
//! `delegate_task` spawns one, that actually answers.
//!
//! WHY IT EXISTS. `delegate_task` was dead for five weeks — `-q` is
//! `--query`, it takes a value, and the argv builder put a flag there,
//! so clap exited 2 before the child booted. Nothing caught it,
//! because every spawn test in delegate.rs points `VELES_BINARY` at a
//! shell script. Those tests prove what the PARENT intended. This one
//! proves the child's fate: the real binary, the real argv builder,
//! the real boot path, the real turn loop, a real provider wire, and a
//! real persisted transcript at the end of it.
//!
//! HERMETIC AND FREE. The "provider" is a TcpListener in this process
//! speaking one OpenAI-shaped SSE completion, named by a scratch
//! config.toml under a scratch VELES_HOME. No network, no key, no
//! tokens. If this test ever needs either, it has stopped being this
//! test.

use std::io::Write as _;
use std::net::TcpListener;

/// One canned chat.completions stream, served to the first caller.
/// Blocking and single-shot on purpose — a child makes exactly one
/// provider call, and a thread that ends when it does leaks nothing.
fn stub_provider(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub provider");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let Ok(mut s) = stream else { continue };
            // Read the WHOLE request, head and Content-Length body, before
            // answering. A reply sent while request bytes are still unread,
            // followed by this socket's close, makes the kernel send the
            // child RST instead of FIN, and on Linux an RST discards what
            // of the reply the child has not read yet: "sse: error decoding
            // response body", seen once in the nix sandbox (2026-10-20) and
            // never on the host, where the race is lost. Pitfall 87.
            use std::io::Read as _;
            let mut req = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head_len, body_len) = loop {
                let n = match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break (req.len(), 0),
                    Ok(n) => n,
                };
                req.extend_from_slice(&chunk[..n]);
                if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&req[..end]);
                    let len = head
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (end + 4, len);
                }
            };
            while req.len() < head_len + body_len {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&chunk[..n]),
                }
            }
            eprintln!(
                "stub provider: request {} bytes (head {head_len}, body {body_len})",
                req.len()
            );
            let body = format!(
                concat!(
                    "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\"}}}}]}}\n\n",
                    "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"thinking\"}}}}]}}\n\n",
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

fn scratch_home(base_url: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-child-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    // VELES_HOME forces the legacy single-directory layout, so config
    // and data are both this directory — which is what a test wants.
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
fn a_delegate_child_boots_answers_and_leaves_a_transcript() {
    let url = stub_provider("the child answered");
    let home = scratch_home(&url);

    // The child SESSION the parent would have minted, in the same
    // store the child will open — this is what `--resume` resumes
    // into, and the row it must leave its transcript in.
    let child = "20260101_000000_childee";
    {
        let store = veles_core::store::Store::open(&home.join("state.db")).expect("store");
        store.create_session("e2e-parent").expect("parent");
        store
            .create_child_session(child, "e2e-parent", Some("subagent: answer me"))
            .expect("child row");
    }

    // THE REAL ARGV, from the real builder.
    let argv = veles_core::delegate::child_argv("answer me", Some(child), None, None, None);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_veles"))
        .args(&argv)
        .env("VELES_HOME", &home)
        .env("VELES_CHILD_DEPTH", "1")
        .output()
        .expect("spawn the real binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the child exited {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status.code()
    );
    // -Q is the machine-readable contract delegate_task reads: stdout
    // carries ONLY the final reply.
    assert!(
        stdout.contains("the child answered"),
        "the parent reads the child's summary off stdout: {stdout:?}"
    );

    // …and the transcript is in the session the parent can attach to,
    // which is the other half of the taxonomy promise ("children are
    // child sessions … available to attach and review").
    let store = veles_core::store::Store::open(&home.join("state.db")).expect("store");
    let hist = store.load_history(child).expect("child history");
    assert!(
        hist.iter().any(|m| m.content.contains("answer me")),
        "the child's own session holds the task it was given: {hist:?}"
    );
    assert!(
        hist.iter()
            .any(|m| m.content.contains("the child answered")),
        "…and the answer it gave: {hist:?}"
    );
    assert_eq!(
        store.session_parent(child).ok().flatten().as_deref(),
        Some("e2e-parent"),
        "and it is still linked to the session that spawned it"
    );

    let _ = std::fs::remove_dir_all(&home);
}
