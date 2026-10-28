//! lsp_proxy — run a language server on the GATEWAY and pipe it to the
//! desktop.
//!
//! WHY
//!
//! `lspclient.tcl` spawned `tcl-lsp` as a local subprocess of `wish`.
//! That was fine while the desktop and the files were on the same
//! machine. Once the desktop can point at a remote gateway they are not:
//! the Dev pane reads its buffer over `/api/fs/read`, from the GATEWAY's
//! filesystem, and hands the text to a language server running here.
//!
//! It half-worked, which is why it was easy to misdiagnose. The sync is
//! by full document TEXT, not by path, and `rootUri` was `null` — so
//! semantic tokens, diagnostics, hover and completion were all correct
//! against a remote buffer. Only the `file://` URI was a lie, naming a
//! path on the wrong machine.
//!
//! What genuinely did not work was anything needing the PROJECT: no
//! cross-file definitions, no workspace symbols, no references. And that
//! did not work locally either, because `rootUri: null` means the server
//! never had a project to index. USER: "it says lsp can only work
//! locally which seems stupid? Just run the lsp on the remote."
//!
//! So the server runs beside the files, and gets a real root.
//!
//! WHAT THIS IS NOT
//!
//! It is not an LSP client. It does not parse JSON-RPC, track request
//! ids, or interpret a single field. `lspclient.tcl` already does all of
//! that — id correlation, the pending map, `_dispatch`, the capability
//! probe, and every result normaliser, the last of which `lsptest.tcl`
//! pins with 60 assertions. Re-implementing any of it here would be
//! throwing away tested code to write untested code.
//!
//! This is a framed pipe:
//!
//!   POST /api/lsp/start   spawn the server with cwd = the project root
//!   POST /api/lsp/send    body is one JSON-RPC message → framed to stdin
//!   GET  /api/lsp/events  SSE, one event per frame off stdout, verbatim
//!   POST /api/lsp/stop    kill it
//!
//! Every frame goes out, responses and notifications alike — the client
//! is the one that knows a `publishDiagnostics` from a `textDocument/
//! hover` reply.
//!
//! FRAMING is `Content-Length: <n>\r\n\r\n<body>`, and `n` is a BYTE
//! count, not a character count. `lspclient.tcl:50` documents that as a
//! desync it already hit once; the reader here has the same requirement
//! in the other direction, which is why `read_frame` works on bytes
//! throughout and only converts to a string at the end.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

/// Frames off the server's stdout, fanned out to every attached stream.
/// 512 is generous: a burst of `publishDiagnostics` on a big file is the
/// worst case and it is nothing like that many.
const FRAME_CHANNEL: usize = 512;

/// Ceiling on the read buffer while a frame is still incomplete.
///
/// Class: WeeChat WSA-2026-3, "missing limit on unterminated IRC
/// messages causes memory exhaustion". The IRC version of that bug and
/// this one are the same bug: read into a growable buffer, stop when
/// the message is complete, and never ask what happens if it never is.
/// Here a `Content-Length` larger than the server will ever deliver
/// means `take_frame` returns None forever while `buf` keeps growing.
///
/// 16 MiB is enormous for one JSON-RPC message — the largest real ones
/// are `textDocument/didOpen` carrying a whole file, and `semanticTokens`
/// on a big file — so crossing it means the server is broken or the
/// stream has desynced, and in both cases the session is already lost.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// One running language server.
struct Session {
    root: String,
    cmd: String,
    /// The arguments the server was spawned with. Part of the identity a
    /// reuse check compares: `tcl-lsp` and `tcl-lsp --verbose` are not the
    /// same server even for the same root.
    args: Vec<String>,
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    /// Holding the Child keeps the process alive; `kill_on_drop` means
    /// replacing this Session reaps the old server rather than leaking it.
    child: tokio::process::Child,
}

struct State {
    session: Option<Session>,
    tx: broadcast::Sender<String>,
}

fn state() -> &'static Mutex<State> {
    static S: std::sync::OnceLock<Mutex<State>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(FRAME_CHANNEL);
        Mutex::new(State { session: None, tx })
    })
}

/// Attach to the frame stream. Frames published before this call are not
/// replayed — an LSP session has no meaningful backlog, unlike IRC: a
/// response to a request nobody is waiting for any more is noise, and a
/// reattaching client re-runs `initialize` anyway.
pub fn subscribe() -> broadcast::Receiver<String> {
    state().lock().expect("lsp state").tx.subscribe()
}

pub fn is_running() -> bool {
    state()
        .lock()
        .expect("lsp state")
        .session
        .as_ref()
        .is_some()
}

/// The root and command of the running server, if any.
pub fn current() -> Option<(String, String)> {
    let s = state().lock().expect("lsp state");
    s.session.as_ref().map(|x| (x.root.clone(), x.cmd.clone()))
}

/// Wrap one JSON-RPC message in an LSP frame.
///
/// Content-Length is the BYTE length of the UTF-8 body. Using the
/// character count desyncs the stream the moment the body carries a
/// non-ASCII byte — a docstring with an em dash is enough.
pub fn frame(body: &str) -> Vec<u8> {
    let bytes = body.as_bytes();
    let mut out = format!("Content-Length: {}\r\n\r\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out
}

/// Pull one complete frame out of `buf`, returning it and how many bytes
/// to consume. `None` means "not yet — read more".
///
/// Deliberately byte-oriented: the body may contain a literal `\r\n\r\n`
/// inside a JSON string, so the body is taken by LENGTH and never by
/// searching for a terminator.
pub fn take_frame(buf: &[u8]) -> Option<(String, usize)> {
    const SEP: &[u8] = b"\r\n\r\n";
    let head_end = buf.windows(SEP.len()).position(|w| w == SEP)?;
    let header = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
    let len: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())?;
    let body_at = head_end + SEP.len();
    let end = body_at.checked_add(len)?;
    if buf.len() < end {
        return None;
    }
    Some((String::from_utf8_lossy(&buf[body_at..end]).to_string(), end))
}

/// Spawn a language server rooted at `root` — or reuse the one already
/// serving exactly that project.
///
/// Reuse is not a nicety, it is correctness: the Dev pane's root-switch
/// probe POSTs `/api/lsp/start` on every buffer switch, and a start that
/// replaced unconditionally KILLED the live, initialized session and left
/// the client talking to an uninitialized corpse (no handshake runs on
/// the reuse path — the new child just blocks in `initialize_start`).
/// Every tab switch was a respawn + a dead-hover window. That was the
/// 2026-08-31 "sometimes even in non-plugin sources the hover shows
/// nothing". A replaced server is still correct when the root or command
/// actually changes; same triple means same project means same server.
/// A zombie (child that has already exited) does NOT count as alive: a
/// respawn then heals the session instead of returning it as an error.
pub async fn start(cmd: &str, args: &[String], root: &str) -> Result<(), String> {
    {
        let mut s = state().lock().expect("lsp state");
        if let Some(sess) = s.session.as_mut() {
            if sess.cmd == cmd && sess.args == *args && sess.root == root {
                match sess.child.try_wait() {
                    Ok(None) => {
                        tracing::debug!("lsp: reusing server rooted at {root}");
                        return Ok(());
                    }
                    Ok(Some(_)) => {
                        tracing::warn!("lsp: server for {root} exited — respawning");
                    }
                    Err(e) => {
                        tracing::warn!("lsp: try_wait failed for {root}: {e} — respawning");
                    }
                }
            }
        }
        // Replace (or clear the zombie): dropping the old Session reaps it
        // via kill_on_drop.
        s.session = None;
    }
    let root_path = std::path::Path::new(root);
    if !root_path.is_dir() {
        return Err(format!("root is not a directory: {root}"));
    }
    let mut c = tokio::process::Command::new(crate::shell::resolve_program(cmd));
    c.args(args)
        .current_dir(root_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // The server's own diagnostics go to the gateway's stderr, where
        // `/api/logs` already shows them. Discarding them would make "the
        // server started and then said nothing" unexplainable.
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    let mut child = c.spawn().map_err(|e| format!("spawn {cmd}: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;

    let tx = { state().lock().expect("lsp state").tx.clone() };
    crate::rt::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break, // server exited
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > MAX_FRAME_BYTES {
                        tracing::warn!(
                            "lsp: {} bytes buffered without a complete frame — dropping the \
                             session rather than growing further",
                            buf.len()
                        );
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("lsp: read failed: {e}");
                    break;
                }
            }
            // A single read can carry several frames, or half of one.
            while let Some((body, used)) = take_frame(&buf) {
                buf.drain(..used);
                // Every subscriber gone is the normal case when the Dev
                // pane is closed; the server stays up for the next one.
                let _ = tx.send(body);
            }
        }
        tracing::info!("lsp: server stream ended");
    });

    let mut s = state().lock().expect("lsp state");
    s.session = Some(Session {
        root: root.to_string(),
        cmd: cmd.to_string(),
        args: args.to_vec(),
        stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
        child,
    });
    Ok(())
}

/// Frame one JSON-RPC message to the server's stdin.
pub async fn send(body: &str) -> Result<(), String> {
    // Clone the handle out of the std Mutex before awaiting: holding a
    // std guard across an await point is the clippy lint and the deadlock
    // both.
    let stdin = {
        let s = state().lock().expect("lsp state");
        match s.session.as_ref() {
            Some(sess) => sess.stdin.clone(),
            None => return Err("no language server running — POST /api/lsp/start first".into()),
        }
    };
    let bytes = frame(body);
    let mut w = stdin.lock().await;
    w.write_all(&bytes)
        .await
        .map_err(|e| format!("write: {e}"))?;
    w.flush().await.map_err(|e| format!("flush: {e}"))
}

/// Stop the server. Dropping the Session drops the Child, which was
/// spawned `kill_on_drop`.
pub fn stop() -> bool {
    let mut s = state().lock().expect("lsp state");
    s.session.take().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_counts_bytes_not_characters() {
        // The desync lspclient.tcl documents, in the other direction. An
        // em dash is three bytes and one character; a header built from
        // the character count would leave two bytes of this frame at the
        // head of the next one, and every frame after it would be junk.
        let body = r#"{"m":"a—b"}"#;
        assert_eq!(body.chars().count(), 11);
        assert_eq!(body.len(), 13);
        let f = frame(body);
        let head = String::from_utf8_lossy(&f[..f.len() - body.len()]).to_string();
        assert_eq!(head, "Content-Length: 13\r\n\r\n");
        let (got, used) = take_frame(&f).expect("one frame");
        assert_eq!(got, body);
        assert_eq!(used, f.len(), "the whole frame is consumed");
    }

    #[test]
    fn a_body_containing_the_header_separator_survives() {
        // Taking the body by searching for \r\n\r\n instead of by length
        // truncates this one. A server sending a document's text back in
        // a hover or a code action does exactly this.
        let body = "{\"text\":\"line\\r\\n\\r\\nnext\"}";
        let (got, _) = take_frame(&frame(body)).expect("one frame");
        assert_eq!(got, body);
    }

    #[test]
    fn frames_accumulate_across_partial_reads() {
        let a = frame(r#"{"id":1}"#);
        let b = frame(r#"{"id":2}"#);
        let mut wire = a.clone();
        wire.extend_from_slice(&b);

        // Nothing yet on a partial header.
        assert!(take_frame(&wire[..10]).is_none());
        // Nothing yet on a complete header with a partial body.
        assert!(take_frame(&wire[..a.len() - 1]).is_none());

        // Both frames come out of one buffer, in order.
        let mut buf = wire.clone();
        let (first, used) = take_frame(&buf).expect("first");
        assert_eq!(first, r#"{"id":1}"#);
        buf.drain(..used);
        let (second, used) = take_frame(&buf).expect("second");
        assert_eq!(second, r#"{"id":2}"#);
        buf.drain(..used);
        assert!(buf.is_empty());
    }

    #[test]
    fn a_header_without_content_length_is_not_a_frame() {
        assert!(take_frame(b"Content-Type: x\r\n\r\nhello").is_none());
        assert!(take_frame(b"Content-Length: nonsense\r\n\r\nhello").is_none());
    }

    #[test]
    fn the_header_is_case_insensitive() {
        // Some servers send `content-length`. The spec says the header is
        // case-insensitive and a client that assumes otherwise hangs
        // waiting for a frame it has already received.
        let mut wire = b"content-length: 8\r\n\r\n".to_vec();
        wire.extend_from_slice(br#"{"id":1}"#);
        let (got, _) = take_frame(&wire).expect("lowercase header");
        assert_eq!(got, r#"{"id":1}"#);
    }

    /// The state under test is process-global, and one test that spawns a
    /// live server would let another test's `send` succeed against it.
    /// Session-touching tests serialize on this lock. It is deliberately
    /// a `tokio::sync::Mutex`, not the std one: its guard is designed to
    /// be held across `.await`, which is exactly what these async tests
    /// do (the std guard would trip await_holding_lock — rightly, since
    /// the whole file exists to say "do not hold std locks across
    /// awaits").
    async fn session_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
    }

    #[tokio::test]
    async fn sending_before_starting_says_so_rather_than_panicking() {
        let _guard = session_guard().await;
        let err = send(r#"{"id":1}"#).await.unwrap_err();
        assert!(
            err.contains("/api/lsp/start"),
            "the error must name the fix, got: {err}"
        );
    }

    #[tokio::test]
    async fn starting_in_a_non_directory_is_an_error_not_a_spawn() {
        let _guard = session_guard().await;
        let err = start("tcl-lsp", &[], "/definitely/not/here")
            .await
            .unwrap_err();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[tokio::test]
    async fn same_project_start_twice_reuses_the_running_server() {
        // The 2026-08-31 hover-storm regression: the Dev pane probes
        // /api/lsp/start on every buffer switch, and a start that replaced
        // unconditionally killed the initialized session, leaving the
        // client talking to a never-handshaked corpse. Same
        // (cmd, args, root) must be a no-op against a live child.
        let _guard = session_guard().await;
        // A process that lives long and reads nothing: `sleep` on unix,
        // `ping -n <seconds>` on Windows (one ping a second). The root
        // must exist: the temp dir, not `/tmp`.
        let (cmd, long, short) = if cfg!(windows) {
            (
                "ping",
                vec!["-n".to_string(), "600".to_string(), "127.0.0.1".to_string()],
                vec!["-n".to_string(), "300".to_string(), "127.0.0.1".to_string()],
            )
        } else {
            ("sleep", vec!["600".to_string()], vec!["300".to_string()])
        };
        let root = std::env::temp_dir().display().to_string();
        start(cmd, &long, &root)
            .await
            .expect("spawn sleep as the fake server");
        let first = {
            let s = state().lock().expect("lsp state");
            s.session.as_ref().expect("session up").child.id()
        };
        assert!(first.is_some(), "child has a pid while alive");

        start(cmd, &long, &root)
            .await
            .expect("second start of the same project");
        let second = {
            let s = state().lock().expect("lsp state");
            s.session.as_ref().expect("session still up").child.id()
        };
        assert_eq!(first, second, "reuse must keep the SAME process");

        // A different command line IS a different server: replace, reap.
        start(cmd, &short, &root)
            .await
            .expect("start after changing args");
        let third = {
            let s = state().lock().expect("lsp state");
            s.session.as_ref().expect("session up").child.id()
        };
        assert_ne!(first, third, "a new spawn has a new pid");
        assert!(stop(), "the session must have been up to stop");
        assert!(!stop(), "and now it is down");
    }
}
