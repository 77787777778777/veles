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
//!                         (which server, and which root, is
//!                         `lsp_servers`' decision, by file type)
//!   POST /api/lsp/send    body is one JSON-RPC message → framed to stdin
//!   GET  /api/lsp/events  SSE, one event per frame off stdout, verbatim
//!   POST /api/lsp/stop    kill it
//!
//! Each takes `?session=<key>` (the start reply names it); without one
//! they mean the most recently started session.
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

//
// ONE SERVER PER LANGUAGE (0.2.0, owner ask 287976.2)
//
// There used to be exactly one session, so a tab switch from a Tcl
// plugin to a Rust plugin crate killed tcl-lsp, and switching back
// killed rust-analyzer mid-index. Sessions are now keyed by the server
// family `lsp_servers::Language::server_key` names (`tcl`, `typescript`,
// `rust`, `c`, `racket`, `lisp`); each key keeps the old single-session
// rules (reuse on the same command and root, replace otherwise). A
// request that names no key gets the most recently started one, which
// is what a 0.1.x desktop — Tcl only — always meant.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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

/// How long a fresh server is watched for dying at once. A program that
/// is on PATH but cannot run — the rustup proxy for a toolchain without
/// rust-analyzer, `racket -l racket-langserver` without the package —
/// exits within this, and its own words are the clearest message the
/// editor can show. A healthy server is still starting up.
const EARLY_EXIT: std::time::Duration = std::time::Duration::from_millis(400);

/// Lines of a server's stderr kept for an exit message.
const STDERR_TAIL: usize = 12;

/// The session key a request that names none means (0.1.x desktops).
pub const DEFAULT_KEY: &str = "tcl";

/// What the frame stream carries.
#[derive(Debug, Clone)]
pub enum Frame {
    /// One JSON-RPC message, de-framed, verbatim.
    Message(String),
    /// The server process ended on its own. `generation` tells a stale
    /// notice (from a server already replaced) from a current one.
    Exited { generation: u64, reason: String },
}

/// One running language server.
struct Session {
    root: String,
    cmd: String,
    /// The arguments the server was spawned with. Part of the identity a
    /// reuse check compares: `tcl-lsp` and `tcl-lsp --verbose` are not the
    /// same server even for the same root.
    args: Vec<String>,
    /// The initialize response for the current initialized process. This is
    /// replayed only to a newly attached client, not as a general event log.
    initialize_result: Arc<std::sync::OnceLock<String>>,
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    /// Holding the Child keeps the process alive; `kill_on_drop` means
    /// replacing this Session reaps the old server rather than leaking it.
    child: tokio::process::Child,
    generation: u64,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
    /// One channel per key, kept across server replacements: a client
    /// attached to `rust` keeps its stream when rust-analyzer is re-rooted,
    /// exactly as the single global channel behaved before.
    channels: HashMap<String, broadcast::Sender<Frame>>,
    last: Option<String>,
    next_generation: u64,
}

impl State {
    fn channel(&mut self, key: &str) -> broadcast::Sender<Frame> {
        self.channels
            .entry(key.to_string())
            .or_insert_with(|| broadcast::channel(FRAME_CHANNEL).0)
            .clone()
    }
}

fn state() -> &'static Mutex<State> {
    static S: std::sync::OnceLock<Mutex<State>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// The session a request means: the key it names, else the most recently
/// started one, else [`DEFAULT_KEY`].
pub fn resolve_key(requested: Option<&str>) -> String {
    if let Some(key) = requested.map(str::trim).filter(|k| !k.is_empty()) {
        return key.to_string();
    }
    state()
        .lock()
        .expect("lsp state")
        .last
        .clone()
        .unwrap_or_else(|| DEFAULT_KEY.to_string())
}

/// Attach to one session's frame stream. Frames published before this
/// call are not replayed — an LSP session has no meaningful backlog,
/// unlike IRC: a response to a request nobody is waiting for any more is
/// noise, and a reattaching client re-runs `initialize` anyway.
pub fn subscribe(key: &str) -> broadcast::Receiver<Frame> {
    state().lock().expect("lsp state").channel(key).subscribe()
}

pub fn is_running(key: &str) -> bool {
    state()
        .lock()
        .expect("lsp state")
        .sessions
        .contains_key(key)
}

/// One running session, for `/api/lsp/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub key: String,
    pub root: String,
    pub cmd: String,
}

/// Every running session, most recently started first.
pub fn current() -> Vec<SessionInfo> {
    let s = state().lock().expect("lsp state");
    let mut out: Vec<SessionInfo> = s
        .sessions
        .iter()
        .map(|(key, x)| SessionInfo {
            key: key.clone(),
            root: x.root.clone(),
            cmd: x.cmd.clone(),
        })
        .collect();
    out.sort_by_key(|info| Some(&info.key) != s.last.as_ref());
    out
}

/// Whether an exit notice is about the server that is current for `key`
/// (and not one a later start already replaced or a stop removed).
pub fn exit_is_current(key: &str, generation: u64) -> bool {
    state()
        .lock()
        .expect("lsp state")
        .sessions
        .get(key)
        .is_some_and(|s| s.generation == generation)
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

/// Whether `start` spawned a fresh process or reused its live, already
/// initialized one. A client must initialize only a fresh process; sending
/// initialize to a reused tcl-lsp reaches its ordinary request loop, which
/// answers null and makes its advertised capabilities look absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    Reused,
}

type Tail = Arc<Mutex<VecDeque<String>>>;

fn tail_text(tail: &Tail) -> String {
    tail.lock()
        .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// Spawn a language server rooted at `root` for session `key` — or reuse
/// the one already serving exactly that project.
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
pub async fn start(
    key: &str,
    cmd: &str,
    args: &[String],
    root: &str,
) -> Result<StartOutcome, String> {
    {
        let mut s = state().lock().expect("lsp state");
        s.last = Some(key.to_string());
        if let Some(sess) = s.sessions.get_mut(key) {
            if sess.cmd == cmd && sess.args == *args && sess.root == root {
                match sess.child.try_wait() {
                    // Alive but never initialized (e.g. the desktop's
                    // re-root probe spawned it a moment ago): keep the
                    // process, but the caller is its first client and must
                    // send initialize; there is nothing to replay.
                    Ok(None) if sess.initialize_result.get().is_none() => {
                        tracing::debug!("lsp[{key}]: live uninitialized server at {root}");
                        return Ok(StartOutcome::Started);
                    }
                    Ok(None) => {
                        tracing::debug!("lsp[{key}]: reusing server rooted at {root}");
                        return Ok(StartOutcome::Reused);
                    }
                    Ok(Some(_)) => {
                        tracing::warn!("lsp[{key}]: server for {root} exited — respawning");
                    }
                    Err(e) => {
                        tracing::warn!("lsp[{key}]: try_wait failed for {root}: {e} — respawning");
                    }
                }
            }
        }
        // Replace (or clear the zombie): dropping the old Session reaps it
        // via kill_on_drop.
        s.sessions.remove(key);
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
        // Piped, then copied line by line to the gateway's own stderr
        // (where `/api/logs` shows it, as before) — and the last lines are
        // kept, so a server that dies says why in the editor instead of
        // "the server started and then said nothing".
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = c.spawn().map_err(|e| format!("spawn {cmd}: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let tail: Tail = Arc::new(Mutex::new(VecDeque::new()));
    {
        let tail = tail.clone();
        let name = cmd.to_string();
        crate::rt::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{name}: {line}");
                if let Ok(mut t) = tail.lock() {
                    if t.len() == STDERR_TAIL {
                        t.pop_front();
                    }
                    t.push_back(line.chars().take(300).collect());
                }
            }
        });
    }
    // Dead on arrival: report it as the start's error, in the server's
    // own words. `wait` is cancel-safe, so a live server simply keeps
    // running when the window closes.
    if let Ok(Ok(status)) = tokio::time::timeout(EARLY_EXIT, child.wait()).await {
        // Let the stderr copier drain what the process wrote last.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let said = tail_text(&tail);
        return Err(if said.is_empty() {
            format!("{cmd} exited at once ({status}) on the gateway")
        } else {
            format!("{cmd} exited at once ({status}) on the gateway: {said}")
        });
    }

    let (tx, generation) = {
        let mut s = state().lock().expect("lsp state");
        s.next_generation += 1;
        (s.channel(key), s.next_generation)
    };
    let initialize_result = Arc::new(std::sync::OnceLock::new());
    let captured_initialize = initialize_result.clone();
    let exit_tail = tail.clone();
    let name = cmd.to_string();
    let log_key = key.to_string();
    crate::rt::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let reason = loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break "the server closed its output".to_string(),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > MAX_FRAME_BYTES {
                        tracing::warn!(
                            "lsp[{log_key}]: {} bytes buffered without a complete frame — \
                             dropping the session rather than growing further",
                            buf.len()
                        );
                        break "the server's output desynced (an oversized frame)".to_string();
                    }
                }
                Err(e) => {
                    tracing::warn!("lsp[{log_key}]: read failed: {e}");
                    break format!("reading the server failed: {e}");
                }
            }
            // A single read can carry several frames, or half of one.
            while let Some((body, used)) = take_frame(&buf) {
                buf.drain(..used);
                capture_initialize_result(&body, &captured_initialize);
                // Every subscriber gone is the normal case when the Dev
                // pane is closed; the server stays up for the next one.
                let _ = tx.send(Frame::Message(body));
            }
        };
        tracing::info!("lsp[{log_key}]: server stream ended");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let said = tail_text(&exit_tail);
        let reason = if said.is_empty() {
            format!("{name} stopped: {reason}")
        } else {
            format!("{name} stopped: {reason}; it said: {said}")
        };
        let _ = tx.send(Frame::Exited { generation, reason });
    });

    let mut s = state().lock().expect("lsp state");
    s.sessions.insert(
        key.to_string(),
        Session {
            root: root.to_string(),
            cmd: cmd.to_string(),
            args: args.to_vec(),
            initialize_result,
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            child,
            generation,
        },
    );
    Ok(StartOutcome::Started)
}

/// The initialize response for the current process, if it has completed its
/// handshake. A reconnecting client gets this instead of sending initialize
/// to a running tcl-lsp a second time (which answers null).
pub fn initialize_result(key: &str) -> Option<String> {
    let s = state().lock().expect("lsp state");
    s.sessions
        .get(key)
        .and_then(|session| session.initialize_result.get().cloned())
}

/// Record the first initialize response. Later responses with the same id
/// are not initialization evidence; an actual re-initialize is refused by
/// tcl-lsp with null and must never erase the valid snapshot.
fn capture_initialize_result(body: &str, slot: &std::sync::OnceLock<String>) {
    if slot.get().is_some() || body.len() > 256 * 1024 {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    if value.get("id").and_then(serde_json::Value::as_i64) != Some(1)
        || value.get("method").is_some()
        || !value
            .get("result")
            .and_then(|result| result.get("capabilities"))
            .is_some_and(serde_json::Value::is_object)
    {
        return;
    }
    let _ = slot.set(body.to_string());
}

/// Frame one JSON-RPC message to session `key`'s stdin.
pub async fn send(key: &str, body: &str) -> Result<(), String> {
    // Clone the handle out of the std Mutex before awaiting: holding a
    // std guard across an await point is the clippy lint and the deadlock
    // both.
    let stdin = {
        let s = state().lock().expect("lsp state");
        match s.sessions.get(key) {
            Some(sess) => sess.stdin.clone(),
            None => {
                return Err(format!(
                    "no {key} language server running — POST /api/lsp/start first"
                ))
            }
        }
    };
    let bytes = frame(body);
    let mut w = stdin.lock().await;
    w.write_all(&bytes)
        .await
        .map_err(|e| format!("write: {e}"))?;
    w.flush().await.map_err(|e| format!("flush: {e}"))
}

/// Stop one session, or every session when `key` is None. Dropping the
/// Session drops the Child, which was spawned `kill_on_drop`.
pub fn stop(key: Option<&str>) -> bool {
    let mut s = state().lock().expect("lsp state");
    match key {
        Some(key) => {
            let was = s.sessions.remove(key).is_some();
            if s.last.as_deref() == Some(key) {
                s.last = None;
            }
            was
        }
        None => {
            let was = !s.sessions.is_empty();
            s.sessions.clear();
            s.last = None;
            was
        }
    }
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
        let err = send("never-started", r#"{"id":1}"#).await.unwrap_err();
        assert!(
            err.contains("/api/lsp/start"),
            "the error must name the fix, got: {err}"
        );
    }

    #[tokio::test]
    async fn starting_in_a_non_directory_is_an_error_not_a_spawn() {
        let _guard = session_guard().await;
        let err = start("tcl", "tcl-lsp", &[], "/definitely/not/here")
            .await
            .unwrap_err();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn initialize_replay_is_bounded_and_never_replaced_by_a_null_reinitialize() {
        let slot = std::sync::OnceLock::new();
        let valid =
            r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"definitionProvider":true}}}"#;
        capture_initialize_result(valid, &slot);
        assert_eq!(slot.get().map(String::as_str), Some(valid));
        capture_initialize_result(r#"{"jsonrpc":"2.0","id":1,"result":null}"#, &slot);
        capture_initialize_result(
            r#"{"jsonrpc":"2.0","id":2,"result":{"capabilities":{}}}"#,
            &slot,
        );
        capture_initialize_result(
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics"}"#,
            &slot,
        );
        assert_eq!(slot.get().map(String::as_str), Some(valid));
    }

    /// A process that lives long and reads nothing: `sleep` on unix,
    /// `ping -n <seconds>` on Windows (one ping a second).
    fn idle(seconds: u32) -> (&'static str, Vec<String>) {
        if cfg!(windows) {
            (
                "ping",
                vec!["-n".into(), seconds.to_string(), "127.0.0.1".into()],
            )
        } else {
            ("sleep", vec![seconds.to_string()])
        }
    }

    fn pid(key: &str) -> Option<u32> {
        let s = state().lock().expect("lsp state");
        s.sessions.get(key).and_then(|x| x.child.id())
    }

    #[tokio::test]
    async fn same_project_start_twice_reuses_the_running_server() {
        // The 2026-08-31 hover-storm regression: the Dev pane probes
        // /api/lsp/start on every buffer switch, and a start that replaced
        // unconditionally killed the initialized session, leaving the
        // client talking to a never-handshaked corpse. Same
        // (cmd, args, root) must be a no-op against a live child.
        let _guard = session_guard().await;
        let (cmd, long) = idle(600);
        let (_, short) = idle(300);
        // The root must exist: the temp dir, not `/tmp`.
        let root = std::env::temp_dir().display().to_string();
        let key = "test-reuse";
        assert_eq!(
            start(key, cmd, &long, &root).await.expect("spawn outcome"),
            StartOutcome::Started
        );
        let first = pid(key);
        assert!(first.is_some(), "child has a pid while alive");

        // Alive but never initialized (the re-root probe spawned it and no
        // client has handshaked yet): the same process is kept, but it is
        // reported as Started so the next client sends initialize instead
        // of waiting on a replay that does not exist (dev-ext-6).
        assert_eq!(
            start(key, cmd, &long, &root)
                .await
                .expect("uninitialized outcome"),
            StartOutcome::Started
        );
        {
            let s = state().lock().expect("lsp state");
            let session = s.sessions.get(key).expect("session up");
            assert_eq!(session.child.id(), first, "the live process is kept");
            session
                .initialize_result
                .set(r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#.into())
                .expect("first initialize result");
        }
        assert_eq!(
            start(key, cmd, &long, &root).await.expect("reuse outcome"),
            StartOutcome::Reused
        );
        assert_eq!(first, pid(key), "reuse must keep the SAME process");

        // A different command line IS a different server: replace, reap.
        start(key, cmd, &short, &root)
            .await
            .expect("start after changing args");
        let third = pid(key);
        assert_ne!(first, third, "a new spawn has a new pid");
        assert!(stop(Some(key)), "the session must have been up to stop");
        assert!(!stop(Some(key)), "and now it is down");
    }

    #[tokio::test]
    async fn two_languages_keep_their_own_servers_across_a_tab_switch() {
        // Before 0.2.0 there was one session: opening a Rust plugin crate
        // after a Tcl plugin killed tcl-lsp, and going back killed
        // rust-analyzer mid-index. Each key keeps its own process now,
        // and a request without a key means the most recent start.
        let _guard = session_guard().await;
        let (cmd, a) = idle(600);
        let (_, b) = idle(500);
        let root = std::env::temp_dir().display().to_string();
        start("test-tcl", cmd, &a, &root)
            .await
            .expect("tcl session");
        let tcl = pid("test-tcl");
        start("test-rust", cmd, &b, &root)
            .await
            .expect("rust session");
        assert_eq!(pid("test-tcl"), tcl, "starting rust kept tcl's server");
        assert!(is_running("test-tcl") && is_running("test-rust"));
        assert_eq!(resolve_key(None), "test-rust", "the last start");
        assert_eq!(resolve_key(Some("test-tcl")), "test-tcl");
        let listed = current();
        assert_eq!(listed[0].key, "test-rust", "most recent first: {listed:?}");
        assert!(listed.iter().any(|s| s.key == "test-tcl"));
        // Stopping one session leaves the other.
        assert!(stop(Some("test-rust")));
        assert_eq!(pid("test-tcl"), tcl);
        assert!(stop(Some("test-tcl")));
        assert!(!is_running("test-tcl"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_cannot_run_says_why_in_its_own_words() {
        // `rust-analyzer` on PATH is often the rustup proxy for a
        // toolchain without the component; `racket -l racket-langserver`
        // without the package exits the same way. Both used to be a
        // "started" session that never answered.
        let _guard = session_guard().await;
        let root = std::env::temp_dir().display().to_string();
        let err = start(
            "test-dead",
            "sh",
            &[
                "-c".into(),
                "echo \"error: 'rust-analyzer' is not installed for the toolchain\" >&2; exit 1"
                    .into(),
            ],
            &root,
        )
        .await
        .unwrap_err();
        assert!(err.contains("exited at once"), "{err}");
        assert!(err.contains("is not installed for the toolchain"), "{err}");
        assert!(!is_running("test-dead"), "a dead start is not a session");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_dies_later_sends_one_current_exit_notice() {
        let _guard = session_guard().await;
        let root = std::env::temp_dir().display().to_string();
        let mut rx = subscribe("test-late");
        start(
            "test-late",
            "sh",
            &[
                "-c".into(),
                "sleep 1; echo 'panicked at index' >&2; exit 3".into(),
            ],
            &root,
        )
        .await
        .expect("alive past the early-exit window");
        let notice = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("an exit notice within the deadline")
            .expect("the channel stays open");
        let Frame::Exited { generation, reason } = notice else {
            panic!("expected an exit notice, got {notice:?}");
        };
        assert!(reason.contains("panicked at index"), "{reason}");
        assert!(exit_is_current("test-late", generation));
        // A replacement makes the old notice stale: the SSE stream must not
        // tear down a client of the new server over it.
        let (cmd, args) = idle(600);
        start("test-late", cmd, &args, &root)
            .await
            .expect("respawn");
        assert!(!exit_is_current("test-late", generation));
        assert!(stop(Some("test-late")));
    }

    // ── real servers, through the proxy, the way the Dev pane drives them ──
    //
    // Each test picks its server with `lsp_servers::select_with` (so the
    // file-type and root rules are the ones under test), starts it through
    // `start`, and speaks the client's own handshake: the position
    // encodings the desktop advertises for that language, server→client
    // requests answered, and hover, definition, completion and diagnostics
    // read back. A server that is not installed is a SKIP unless
    // VELES_TEST_LSP_REQUIRE names it (the devShell does).

    fn lookup(name: &str) -> Option<std::path::PathBuf> {
        if let Some(extra) = std::env::var_os("VELES_TEST_LSP_PATH") {
            for dir in std::env::split_paths(&extra) {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        crate::shell::which(name)
    }

    fn need(name: &str) -> Option<std::path::PathBuf> {
        let found = lookup(name);
        if found.is_none() {
            let required = std::env::var("VELES_TEST_LSP_REQUIRE").unwrap_or_default();
            assert!(
                !required.split(',').any(|r| r.trim() == name),
                "{name} is required by VELES_TEST_LSP_REQUIRE but was not found"
            );
            eprintln!("SKIP: {name} is not installed");
        }
        found
    }

    struct Live {
        key: String,
        rx: broadcast::Receiver<Frame>,
        next: i64,
        diagnostics: Vec<serde_json::Value>,
    }

    impl Live {
        async fn send(&self, v: serde_json::Value) {
            send(&self.key, &v.to_string()).await.expect("send");
        }

        /// One request, answering whatever the server asks meanwhile and
        /// keeping every publishDiagnostics it pushes.
        async fn request(
            &mut self,
            method: &str,
            params: serde_json::Value,
            secs: u64,
        ) -> serde_json::Value {
            let id = self.next;
            self.next += 1;
            self.send(
                serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            )
            .await;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
            loop {
                let frame = tokio::time::timeout_at(deadline, self.rx.recv())
                    .await
                    .unwrap_or_else(|_| panic!("{method} got no answer in {secs}s"))
                    .expect("frame stream");
                let body = match frame {
                    Frame::Message(body) => body,
                    Frame::Exited { reason, .. } => panic!("server exited: {reason}"),
                };
                let v: serde_json::Value = serde_json::from_str(&body).expect("server JSON");
                if v.get("method").is_some() {
                    if v["method"] == "textDocument/publishDiagnostics" {
                        self.diagnostics.push(v["params"].clone());
                    }
                    if let Some(sid) = v.get("id") {
                        // The client's answer to server requests
                        // (configuration, registerCapability, progress).
                        let result = if v["method"] == "workspace/configuration" {
                            let n = v["params"]["items"].as_array().map_or(0, Vec::len);
                            serde_json::Value::Array(vec![serde_json::Value::Null; n])
                        } else {
                            serde_json::Value::Null
                        };
                        self.send(
                            serde_json::json!({"jsonrpc": "2.0", "id": sid, "result": result}),
                        )
                        .await;
                    }
                    continue;
                }
                if v["id"] == id {
                    return v;
                }
            }
        }

        /// The first diagnostics published for `uri` that `want` accepts.
        /// A server may publish several sets while it loads (rust-analyzer
        /// a workspace warning before the file's own errors), so the
        /// wait is for the set the test is about, not the first one.
        async fn wait_diagnostics(
            &mut self,
            uri: &str,
            secs: u64,
            want: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
            loop {
                if let Some(d) = self.diagnostics.iter().rev().find(|d| {
                    d["uri"] == uri
                        && !d["diagnostics"].as_array().is_none_or(Vec::is_empty)
                        && want(d)
                }) {
                    return d.clone();
                }
                let frame = tokio::time::timeout_at(deadline, self.rx.recv())
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "no matching diagnostics for {uri} in {secs}s; published: {:?}",
                            self.diagnostics
                        )
                    })
                    .expect("frame stream");
                if let Frame::Message(body) = frame {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                    if v["method"] == "textDocument/publishDiagnostics" {
                        self.diagnostics.push(v["params"].clone());
                    } else if let (Some(_), Some(sid)) = (v.get("method"), v.get("id")) {
                        self.send(serde_json::json!({"jsonrpc": "2.0", "id": sid, "result": null}))
                            .await;
                    }
                }
            }
        }
    }

    /// Start the selected server for `file` and initialize it the way
    /// lspclient.tcl does for that language.
    async fn open_live(file: &std::path::Path, text: &str, key: &str) -> (Live, String, String) {
        let sel = crate::lsp_servers::select_with(file, &lookup, &|_| None).expect("a server");
        let root = sel.root.display().to_string();
        let program = lookup(&sel.program).expect("resolved");
        start(key, &program.to_string_lossy(), &sel.args, &root)
            .await
            .expect("start the server");
        let mut live = Live {
            key: key.to_string(),
            rx: subscribe(key),
            next: 1,
            diagnostics: vec![],
        };
        let os = crate::platform::OS;
        let encodings = if sel.language == crate::lsp_servers::Language::Tcl {
            serde_json::json!(["utf-8"])
        } else {
            serde_json::json!(["utf-32", "utf-16"])
        };
        // Not the client's: rust-analyzer would otherwise run the fixture's
        // `cargo check` into the CARGO_TARGET_DIR this test binary was
        // built in. Hover and definition need neither.
        let options = if sel.language == crate::lsp_servers::Language::Rust {
            serde_json::json!({
                "cargo": {"buildScripts": {"enable": false}},
                "procMacro": {"enable": false},
                "checkOnSave": false
            })
        } else {
            serde_json::Value::Null
        };
        let init = live
            .request(
                "initialize",
                serde_json::json!({
                    "processId": null,
                    "rootUri": crate::lsp_servers::file_uri(&root, os),
                    "initializationOptions": options,
                    // As lspclient.tcl advertises them (pitfall 107:
                    // without publishDiagnostics tsserver publishes none).
                    "capabilities": {
                        "general": {"positionEncodings": encodings},
                        "textDocument": {
                            "hover": {"contentFormat": ["plaintext", "markdown"]},
                            "publishDiagnostics": {"relatedInformation": false}
                        }
                    }
                }),
                60,
            )
            .await;
        assert!(init["result"]["capabilities"].is_object(), "{init}");
        let encoding = init["result"]["capabilities"]["positionEncoding"]
            .as_str()
            .unwrap_or("utf-16")
            .to_string();
        live.send(serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            .await;
        let uri = crate::lsp_servers::file_uri(&file.display().to_string(), os);
        live.send(serde_json::json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": sel.language.id(), "version": 1, "text": text}}
        }))
        .await;
        (live, uri, encoding)
    }

    fn at(uri: &str, line: u64, character: u64) -> serde_json::Value {
        serde_json::json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}})
    }

    fn hover_text(v: &serde_json::Value) -> String {
        let c = &v["result"]["contents"];
        c["value"]
            .as_str()
            .map(str::to_string)
            .or_else(|| c.as_str().map(str::to_string))
            .unwrap_or_else(|| c.to_string())
    }

    fn definition_line(v: &serde_json::Value) -> Option<u64> {
        let r = &v["result"];
        let loc = if r.is_array() { &r[0] } else { r };
        loc["range"]["start"]["line"]
            .as_u64()
            .or_else(|| loc["targetSelectionRange"]["start"]["line"].as_u64())
    }

    impl Live {
        /// Repeat a request until `ready` accepts the answer or `secs`
        /// pass, and return the last answer. A server that is still
        /// loading its project answers null or an empty list (tsserver
        /// for the first few hundred milliseconds after didOpen,
        /// rust-analyzer until cargo metadata and the sysroot are in);
        /// the editor's next request is the retry.
        async fn poll(
            &mut self,
            method: &str,
            params: serde_json::Value,
            secs: u64,
            ready: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            loop {
                let answer = self.request(method, params.clone(), secs).await;
                if ready(&answer) || std::time::Instant::now() > deadline {
                    return answer;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }

    fn completion_labels(v: &serde_json::Value) -> Vec<String> {
        let r = &v["result"];
        let items = r["items"].as_array().or_else(|| r.as_array());
        items
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i["label"].as_str().map(|s| s.trim().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_c_extension_gets_hover_definition_completion_and_diagnostics_from_clangd() {
        if need("clangd").is_none() {
            return;
        }
        let _guard = session_guard().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            "name = \"cplug\"\nkind = \"rust\"\n",
        )
        .unwrap();
        let file = dir.path().join("plugin.c");
        // Line 2 calls `helper` at characters 31..37, after a non-ASCII
        // comment on line 0 (utf-32 is negotiated, so characters are
        // characters). The error is a statement of its own: inside the
        // call's expression it would make the call a recovery node with
        // nothing to hover.
        let text = "/* é€ helper docs */\nstatic int helper(int x) { return x + 1; }\nint veles_entry(void) { return helper(41); }\nint broken(void) { return missing; }\n";
        std::fs::write(&file, text).unwrap();
        let (mut live, uri, encoding) = open_live(&file, text, "live-c").await;
        assert_eq!(encoding, "utf-32", "clangd honours the desktop's utf-32");
        let hover = live
            .request("textDocument/hover", at(&uri, 2, 32), 30)
            .await;
        assert!(hover_text(&hover).contains("helper"), "{hover}");
        let def = live
            .request("textDocument/definition", at(&uri, 2, 32), 30)
            .await;
        assert_eq!(definition_line(&def), Some(1), "{def}");
        let comp = live
            .request("textDocument/completion", at(&uri, 2, 34), 30)
            .await;
        assert!(
            completion_labels(&comp)
                .iter()
                .any(|l| l.starts_with("helper")),
            "{comp}"
        );
        live.wait_diagnostics(&uri, 30, |d| d.to_string().contains("missing"))
            .await;
        assert!(stop(Some("live-c")));
    }

    #[tokio::test]
    async fn a_rust_plugin_crate_gets_hover_definition_completion_and_diagnostics_from_rust_analyzer(
    ) {
        if need("rust-analyzer").is_none() || need("cargo").is_none() {
            return;
        }
        let _guard = session_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let krate = dir.path().join("rplug");
        std::fs::create_dir_all(krate.join("src")).unwrap();
        std::fs::write(
            krate.join("Cargo.toml"),
            "[package]\nname = \"rplug\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[workspace]\n",
        )
        .unwrap();
        let file = krate.join("src/lib.rs");
        // Line 6 calls `helper` at characters 4..10; line 8 is a syntax
        // error rust-analyzer reports by itself (no cargo check runs).
        let text = "/// Adds one.\npub fn helper(x: i32) -> i32 {\n    x + 1\n}\n\npub fn entry() -> i32 {\n    helper(41)\n}\npub fn broken() -> i32 { 1 + }\n";
        std::fs::write(&file, text).unwrap();
        let (mut live, uri, encoding) = open_live(&file, text, "live-rust").await;
        assert_eq!(encoding, "utf-32");
        // rust-analyzer answers null until it has loaded the workspace
        // (cargo metadata, the sysroot).
        let hover = live
            .poll("textDocument/hover", at(&uri, 6, 5), 180, |h| {
                !h["result"].is_null()
            })
            .await;
        assert!(hover_text(&hover).contains("fn helper"), "{hover}");
        let def = live
            .request("textDocument/definition", at(&uri, 6, 5), 60)
            .await;
        assert_eq!(definition_line(&def), Some(1), "{def}");
        // Completion inside `hel|per`: the prefix `hel` offers `helper`.
        let comp = live
            .poll("textDocument/completion", at(&uri, 6, 7), 60, |c| {
                completion_labels(c).iter().any(|l| l.starts_with("helper"))
            })
            .await;
        assert!(
            completion_labels(&comp)
                .iter()
                .any(|l| l.starts_with("helper")),
            "{comp}"
        );
        // The syntax error on line 8.
        live.wait_diagnostics(&uri, 120, |d| {
            d["diagnostics"]
                .as_array()
                .is_some_and(|all| all.iter().any(|x| x["range"]["start"]["line"] == 8))
        })
        .await;
        assert!(stop(Some("live-rust")));
    }

    #[tokio::test]
    async fn a_typescript_extension_gets_hover_definition_completion_diagnostics_and_the_veles_global(
    ) {
        if need("typescript-language-server").is_none() {
            return;
        }
        let _guard = session_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join("tsplug");
        for f in crate::plugins::scaffold_files("tsplug", "probe", "js").unwrap() {
            std::fs::create_dir_all(&ext).unwrap();
            std::fs::write(ext.join(&f.name), f.text).unwrap();
        }
        let file = ext.join("plugin.ts");
        // An astral character before `helper` on line 1: tsserver counts
        // UTF-16 units (it negotiates nothing), which is why the desktop
        // converts from Tk characters (lsptest.tcl pins the conversion).
        // Line 3 is a type error the server must report.
        let text = "function helper(x: number): number { return x + 1; }\nconst s = \"😀\"; const n = helper(1);\nveles.\nconst wrong: number = \"text\";\n";
        std::fs::write(&file, text).unwrap();
        let (mut live, uri, encoding) = open_live(&file, text, "live-ts").await;
        assert_eq!(encoding, "utf-16", "tsserver has no negotiation");
        // `helper` on line 1 is characters 25..31, UTF-16 units 26..32.
        let hover = live
            .poll("textDocument/hover", at(&uri, 1, 27), 60, |h| {
                hover_text(h).contains("function helper")
            })
            .await;
        assert!(hover_text(&hover).contains("function helper"), "{hover}");
        let def = live
            .request("textDocument/definition", at(&uri, 1, 27), 60)
            .await;
        assert_eq!(definition_line(&def), Some(0), "{def}");
        // tsserver answers an empty list until the configured project
        // (tsconfig.json, veles.d.ts) has loaded: a fixed single request
        // here passed or failed on timing.
        let comp = live
            .poll("textDocument/completion", at(&uri, 2, 6), 60, |c| {
                !completion_labels(c).is_empty()
            })
            .await;
        let labels = completion_labels(&comp);
        assert!(
            labels.iter().any(|l| l == "registerTool") && labels.iter().any(|l| l == "hook"),
            "the scaffold's veles.d.ts gives the host API: {labels:?} from {comp}"
        );
        live.wait_diagnostics(&uri, 60, |d| {
            d.to_string().contains("not assignable to type 'number'")
        })
        .await;
        assert!(stop(Some("live-ts")));
    }

    /// tcl-lsp is the operator's own build, not a nixpkgs package, so the
    /// devShell does not require it; on a machine that has it this is the
    /// Tcl leg of the same round trip. It also pins pitfall 105 against
    /// the real server: tcl-lsp's "utf-8" columns are BYTES, so a hover
    /// sent at the Tk character column after "éé€" misses.
    #[tokio::test]
    async fn a_tcl_extension_gets_hover_definition_completion_and_diagnostics_from_tcl_lsp() {
        if need("tcl-lsp").is_none() {
            return;
        }
        let _guard = session_guard().await;
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join("tplug");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("plugin.toml"), "name = \"tplug\"\n").unwrap();
        let file = ext.join("plugin.tcl");
        let text = "proc helper {x} {\n    return [expr {$x + 1}]\n}\nset s \"éé€\"; set n [helper 1]\nhel\nproc broken {a {\n";
        std::fs::write(&file, text).unwrap();
        let (mut live, uri, encoding) = open_live(&file, text, "live-tcl").await;
        assert_eq!(encoding, "utf-8");
        // `helper` on line 3 starts at character 20, byte 24.
        let line3 = text.lines().nth(3).unwrap();
        let byte_col = line3.find("helper").unwrap();
        let char_col = line3[..byte_col].chars().count() as u64;
        let byte_col = byte_col as u64;
        assert_eq!((char_col, byte_col), (20, 24));
        let hover = live
            .request("textDocument/hover", at(&uri, 3, byte_col + 1), 30)
            .await;
        assert!(hover_text(&hover).contains("helper"), "{hover}");
        // Character 22 is inside `helper`; byte 22 is the space before
        // `[`, where tcl-lsp has nothing to say.
        let miss = live
            .request("textDocument/hover", at(&uri, 3, char_col + 2), 30)
            .await;
        assert!(
            !hover_text(&miss).contains("helper"),
            "a character column is not tcl-lsp's column: {miss}"
        );
        let def = live
            .request("textDocument/definition", at(&uri, 3, byte_col + 1), 30)
            .await;
        assert_eq!(definition_line(&def), Some(0), "{def}");
        let comp = live
            .request("textDocument/completion", at(&uri, 4, 3), 30)
            .await;
        assert!(
            completion_labels(&comp).iter().any(|l| l == "helper"),
            "{comp}"
        );
        live.wait_diagnostics(&uri, 30, |d| d.to_string().contains("closing brace"))
            .await;
        assert!(stop(Some("live-tcl")));
    }
}
