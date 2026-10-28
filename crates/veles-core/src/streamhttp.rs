//! Streaming HTTP for provider drivers: the transport `http_post` cannot be.
//!
//! `bridgeops::http_post` buffers. It awaits `resp.text()`, so a driver
//! using it sees a response only once the server has finished producing
//! it, and it carries a 30 second total timeout on top. For a
//! chat-completions gateway answering a short question that is invisible.
//! For a provider driver it is two separate failures:
//!
//!   * no incremental output — the surface waits, then gets everything at
//!     once, so `provider_emit` can only ever fake streaming by emitting
//!     the finished text in one chunk (which is exactly what the reference
//!     Anthropic driver did, and said so in a comment);
//!   * a 30 second ceiling on the whole call — an extended-thinking turn
//!     on a frontier model routinely exceeds it, so the driver dies
//!     mid-thought with a timeout that has nothing to do with the network.
//!
//! This module is the alternative: three ops that hand a driver a real
//! server-sent-event stream, pulled one event at a time from whatever
//! blocking thread the driver runs on.
//!
//! # Shape
//!
//! ```text
//! http_stream_open  <url> <body> ?content_type? ?headers_json?
//!   -> {"id":N,"status":200}          the response status is known
//!   -> {"error":"..."}                the connection itself failed
//! http_stream_next  <id>
//!   -> {"event":"..."}                one SSE `data:` payload
//!   -> {"done":true}                  the server closed cleanly
//!   -> {"error":"..."}                the transport failed mid-stream
//! http_stream_close <id>
//!   -> {"closed":true}                idempotent; frees the handle
//! ```
//!
//! `open` returns as soon as the STATUS is known, not when the body is
//! done: a driver has to classify 429 and 5xx before it decides whether
//! to read a single event, and that classification is what makes plugin
//! routes participate in failover at all.
//!
//! On a non-2xx status the body is still delivered, as one event, and
//! then `done`. So the error path and the success path are the same loop:
//! read the status, pull events until done. A driver does not need a
//! second code path for "the server answered with an error envelope",
//! which matters because Anthropic reports API errors inside a
//! 200-shaped body too.
//!
//! # Why a pull loop and not a callback
//!
//! The socket tier in `soservices` pushes events into a plugin-supplied
//! C callback. That shape cannot be shared by both plugin languages: a
//! Tcl plugin's "callback" is a proc name, which has no meaning across
//! the C ABI (`bridgeops::TCL_ONLY_COMMANDS` exists precisely because of
//! that asymmetry). A pull loop is one op call in both languages — the
//! same as `service_next`, which solved the same problem for service
//! layers. Parity here is a property of the shape, not a promise.
//!
//! # Threading
//!
//! The connection is owned by a task on the BRIDGE's runtime, and events are
//! pushed into an unbounded channel that `next` blocks on. It must be the
//! bridge's runtime and not `crate::rt::spawn`: the caller is a plugin driver
//! on the Tcl interpreter thread, which is a plain std thread with no ambient
//! tokio runtime, and a panic inside an `extern "C"` op shim aborts the
//! process rather than reporting an error (see `Bridge::spawn`).
//! Unbounded is deliberate: the reader task must never block on
//! a slow consumer, or backpressure from one idle driver would stall the
//! socket and eventually trip the server's idle timeout. The cap that
//! keeps this honest is `TOTAL_CAP` below, which bounds the whole stream
//! the way `http_post` bounds a single body.
//!
//! The receiver is locked only for the duration of one `recv`. The lock
//! is never held across the spawn, and the task never touches it, so the
//! two cannot deadlock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

/// What comes out of an open stream.
enum Item {
    /// One SSE event's `data:` payload, as the server framed it.
    Event(String),
    /// The transport failed. Terminal: `Eof` does not follow.
    Error(String),
    /// The server closed cleanly. Terminal.
    Eof,
}

/// One live stream.
///
/// The receiver is `Option` and `next` TAKES it for the duration of its
/// wait, putting it back afterwards. Two reasons, and the first is the
/// load-bearing one: the table lock must not be held across a blocking
/// recv, or every other stream's `next` queues behind the slowest one.
/// The second is that a `None` left behind is a precise answer to "this
/// stream is already being read" — two threads pulling one stream is a
/// driver bug, and saying so beats silently stealing events from each
/// other.
struct OpenStream {
    rx: Mutex<Option<mpsc::UnboundedReceiver<Item>>>,
    task: AbortHandle,
}

/// The whole-stream cap. `http_post` caps a single buffered body at 1 MB
/// (`bridgeops::HTTP_BODY_CAP`); a stream can legitimately be larger than
/// one response because it is many events, so the bound is per-stream and
/// more generous. A stream that exceeds it is cut with an error rather
/// than growing until the process dies — the failure a cap exists to turn
/// into a message.
const TOTAL_CAP: usize = 32 * 1024 * 1024;

/// Per-event bound. One SSE event carrying more than this is not a
/// token stream; it is a server sending something the driver cannot
/// use, and truncating it silently would corrupt JSON the driver is
/// about to parse, so it is an error instead.
const EVENT_CAP: usize = 4 * 1024 * 1024;

/// Time to establish the connection. Separate from the read timeout so
/// that "the host is unreachable" fails fast while "the model is
/// thinking" does not.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest gap between two bytes. Not a total timeout: a frontier model
/// can spend minutes on one turn, and a ceiling on the whole call is
/// exactly what made `http_post` unusable here. Anthropic sends `ping`
/// events while it works, so a live stream always makes progress well
/// inside this window; what it catches is a connection the peer stopped
/// serving without closing, which would otherwise hang the driver (and
/// the turn, and the session) forever.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Streams are process-global, like every other plugin-facing registry:
/// a driver opens one from a blocking thread and pulls from the same
/// thread, and there is no caller context to key it by.
fn table() -> &'static Mutex<HashMap<u64, OpenStream>> {
    static T: OnceLock<Mutex<HashMap<u64, OpenStream>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// How many streams are open. Used by tests and by the doctor surface to
/// prove nothing leaks: every `open` is supposed to end in `close` or in
/// a self-forgetting `Eof`.
pub fn open_streams() -> usize {
    table().lock().map(|t| t.len()).unwrap_or(0)
}

/// Open a streaming POST. Blocks until the response status is known.
///
/// `headers_json` is the same optional JSON object `http_post` takes, and
/// for the same reason: an endpoint that authenticates with something
/// other than a bearer token cannot be reached at all without it.
pub fn open(
    bridge: &crate::bridge::Bridge,
    url: &str,
    body: &str,
    content_type: &str,
    headers_json: Option<&str>,
) -> Result<String, String> {
    let headers = crate::bridgeops::parse_headers_arg(headers_json)?;
    let url = url.to_string();
    let body = body.to_string();
    let content_type = if content_type.trim().is_empty() {
        "application/json".to_string()
    } else {
        content_type.to_string()
    };

    // The item channel exists BEFORE the task is spawned, so an event
    // that arrives while `open` is still waiting on the status oneshot
    // is buffered rather than lost. The task only ever holds the sender.
    let (item_tx, item_rx) = mpsc::unbounded_channel::<Item>();
    let (status_tx, status_rx) = oneshot::channel::<Result<u16, String>>();

    // The BRIDGE's runtime, not `crate::rt::spawn`. The caller here is a
    // plugin driver running on the Tcl interpreter thread, which is a plain
    // std thread with no ambient tokio runtime; `rt::spawn` falls through to
    // `tokio::spawn` in that case and panics with "there is no reactor
    // running". That panic is fatal rather than merely loud, because the op
    // shims in `bridge.rs` are `extern "C"` and cannot unwind — the process
    // aborts. See `Bridge::spawn`.
    let task = bridge.spawn(pump(url, body, content_type, headers, item_tx, status_tx));

    // Block for the status only. The connection and the first headers are
    // what this waits on; the body streams afterwards on the task.
    let status = bridge
        .block_on(status_rx)
        .map_err(|_| "stream closed before a status arrived".to_string())??;

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let abort = task.abort_handle();
    if let Ok(mut t) = table().lock() {
        t.insert(
            id,
            OpenStream {
                rx: Mutex::new(Some(item_rx)),
                task: abort,
            },
        );
    } else {
        // A poisoned table means some other stream panicked while holding
        // it. Drop this connection rather than leak a task nobody can
        // reach, and say so: silently succeeding here would leave the
        // driver pulling from a handle that is not in the table.
        drop(task);
        return Err("stream table is poisoned".to_string());
    }
    Ok(json!({ "id": id, "status": status }).to_string())
}

/// Pull one event. Blocks until one arrives, the stream ends, or it fails.
///
/// A terminal outcome (`done`, or a transport error) frees the handle
/// here rather than waiting for `close`. A driver that returns early on
/// an error — the common shape, since an error means the turn is over —
/// would otherwise leave a task and a socket registered for the life of
/// the process. `close` stays correct and idempotent either way.
pub fn next(bridge: &crate::bridge::Bridge, id: u64) -> Result<String, String> {
    let mut rx = {
        let t = table()
            .lock()
            .map_err(|_| "stream table is poisoned".to_string())?;
        let entry = t
            .get(&id)
            .ok_or_else(|| format!("no open stream {id} (closed, or never opened)"))?;
        let mut guard = entry
            .rx
            .lock()
            .map_err(|_| "stream receiver is poisoned".to_string())?;
        guard
            .take()
            .ok_or_else(|| format!("stream {id} is already being read by another call"))?
    }; // the table lock is dropped HERE, before anything blocks

    let answer = match bridge.block_on(rx.recv()) {
        Some(Item::Event(data)) => {
            put_back(id, rx)?;
            Ok(json!({ "event": data }).to_string())
        }
        // A clean end. The `None` arm is the same fact reached from the
        // other side: the sender dropped without an explicit `Eof`, which
        // only happens if the task returned early — still "nothing more is
        // coming", and treating it as an error would fail a turn whose
        // body arrived complete.
        Some(Item::Eof) | None => {
            drop(rx);
            let _ = close(id)?;
            Ok(json!({ "done": true }).to_string())
        }
        Some(Item::Error(e)) => {
            drop(rx);
            let _ = close(id)?;
            Ok(json!({ "error": e }).to_string())
        }
    };
    answer
}

/// Close a stream and free its handle. Idempotent: closing twice, or
/// closing a stream that ended on its own, is not an error, because the
/// thing a driver needs guaranteed is that nothing is left running.
pub fn close(id: u64) -> Result<String, String> {
    let removed = {
        let mut t = table()
            .lock()
            .map_err(|_| "stream table is poisoned".to_string())?;
        t.remove(&id)
    };
    if let Some(s) = removed {
        s.task.abort();
    }
    Ok(json!({ "closed": true }).to_string())
}

/// Drop every open stream. Called on plugin unload so a driver that
/// returned without closing cannot keep a connection and a task alive
/// past the library that owns it (the host-owns-every-resource rule the
/// socket tier states).
pub fn close_all() -> usize {
    let mut t = match table().lock() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let n = t.len();
    for (_, s) in t.drain() {
        s.task.abort();
    }
    n
}

/// Read the response, then push its SSE events until the peer is done.
async fn pump(
    url: String,
    body: String,
    content_type: String,
    headers: Vec<(String, String)>,
    tx: mpsc::UnboundedSender<Item>,
    status_tx: oneshot::Sender<Result<u16, String>>,
) {
    // A client of its own, deliberately: `http_post` builds one per call
    // with a 30 second TOTAL timeout, and a total timeout is the one
    // setting that must not apply to a stream.
    let client = match reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        // The existing client is built with default-features off, so it
        // does not advertise or decode compression. Staying consistent
        // means a driver sees the same bytes either way.
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = status_tx.send(Err(format!("http client: {e}")));
            return;
        }
    };

    let mut req = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .body(body);
    for (k, v) in headers {
        let name = match reqwest::header::HeaderName::from_bytes(k.as_bytes()) {
            Ok(n) => n,
            Err(_) => {
                let _ = status_tx.send(Err(format!("header name {k:?} is not a valid header")));
                return;
            }
        };
        let value = match reqwest::header::HeaderValue::from_str(&v) {
            Ok(v) => v,
            Err(_) => {
                let _ = status_tx.send(Err(format!("header {k} value is not a valid header")));
                return;
            }
        };
        req = req.header(name, value);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let _ = status_tx.send(Err(format!("http: {e}")));
            return;
        }
    };
    let status = resp.status().as_u16();
    // The status is the whole reason `open` blocks: the driver classifies
    // it before reading a body.
    if status_tx.send(Ok(status)).is_err() {
        return;
    }

    if resp.status().is_success() {
        stream_events(resp, tx).await;
    } else {
        // An error body is delivered as one event and then `done`, so the
        // driver's read loop is the same for both outcomes.
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => format!("(error body unreadable: {e})"),
        };
        let text = if text.len() > EVENT_CAP {
            format!("{}…[truncated]", &text[..EVENT_CAP])
        } else {
            text
        };
        let _ = tx.send(Item::Event(text));
        let _ = tx.send(Item::Eof);
    }
}

/// Decode the SSE frames and push them.
///
/// Framing is `crate::sse::tail_safe`, the same decoder the native
/// streaming clients use — including its promotion of a final event the
/// peer hung without a terminating blank line. That bug cost every reply
/// its tail once already (2026-09-08); a second SSE reader is a second
/// chance to reintroduce it, so there is deliberately not one.
async fn stream_events(resp: reqwest::Response, tx: mpsc::UnboundedSender<Item>) {
    let mut stream = crate::sse::tail_safe(resp.bytes_stream());
    let mut total = 0usize;
    while let Some(ev) = stream.next().await {
        let ev = match ev {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Item::Error(format!("sse: {e}")));
                return;
            }
        };
        if ev.data.len() > EVENT_CAP {
            let _ = tx.send(Item::Error(format!(
                "one event exceeded {EVENT_CAP} bytes; the stream is not a token stream"
            )));
            return;
        }
        total += ev.data.len();
        if total > TOTAL_CAP {
            let _ = tx.send(Item::Error(format!(
                "stream exceeded {TOTAL_CAP} bytes in total; cut rather than grown"
            )));
            return;
        }
        // `[DONE]` is an OpenAI convention, not an SSE one, and Anthropic
        // never sends it. It is passed through as an event rather than
        // swallowed: a driver talking to an OpenAI-shaped gateway needs to
        // see it, and this module's job is framing, not protocol.
        if tx.send(Item::Event(ev.data)).is_err() {
            // The receiver is gone — the driver closed or was unloaded.
            return;
        }
    }
    let _ = tx.send(Item::Eof);
}

/// Put a receiver back after a wait. Split out because `next` returns it
/// on the one non-terminal path, and a path that forgot would leave the
/// stream permanently unreadable while still occupying its handle.
fn put_back(id: u64, rx: mpsc::UnboundedReceiver<Item>) -> Result<(), String> {
    let t = table()
        .lock()
        .map_err(|_| "stream table is poisoned".to_string())?;
    if let Some(entry) = t.get(&id) {
        let mut guard = entry
            .rx
            .lock()
            .map_err(|_| "stream receiver is poisoned".to_string())?;
        *guard = Some(rx);
    }
    // `None` here means the stream was closed by another thread mid-wait.
    // Dropping the receiver then is correct — it closes this end, and the
    // pump task's next `send` sees the channel gone and stops.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // These tests are plain `#[test]`, NOT `#[tokio::test]`, and that is
    // load-bearing: `open`/`next` call `bridge.block_on`, and calling
    // `block_on` from inside an already-running runtime panics. Production
    // reaches these ops from the Tcl interpreter thread — a plain std thread
    // with no ambient runtime — so a sync test is the faithful shape. The
    // server runs on its OWN runtime in its OWN thread.

    /// A fixture server on a dedicated thread with a dedicated runtime.
    ///
    /// The thread is shut down through a channel rather than by counting
    /// connections. Counting is what this used to do, and it hung every test:
    /// a driver that consults its capability catalog makes TWO connections
    /// (a `GET /v1/models` then a `POST /v1/messages`), so a server sized for
    /// one either closed early and the driver got a connection error, or was
    /// sized generously and `Drop` joined a loop that would never end. A
    /// shutdown signal is correct for any number of connections and stops
    /// when the test does.
    struct Server {
        url: String,
        shutdown: Option<std::sync::mpsc::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn url(&self) -> &str {
            &self.url
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            // Signal first, then join: the accept loop is parked in a select
            // that also listens for this, so it returns promptly instead of
            // blocking on the next connection that will never come.
            drop(self.shutdown.take());
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// One response shape: a status, a content type, and a body written in
    /// CHUNKS with a pause between them.
    ///
    /// The chunking is the point. A transport that buffers would still pass a
    /// test whose whole body arrived in one write, and "does this actually
    /// stream incrementally" is the question these tests exist to answer.
    fn server(status: u16, content_type: &str, chunks: Vec<String>) -> Server {
        serve(status, content_type, chunks, false)
    }

    /// Like [`server`], but echoes each request back as a single SSE event —
    /// the way to assert on what actually crossed the socket rather than on
    /// what the driver intended to send.
    fn echo_server() -> Server {
        serve(200, "text/event-stream", Vec::new(), true)
    }

    fn serve(status: u16, content_type: &str, chunks: Vec<String>, echo: bool) -> Server {
        let ct = content_type.to_string();
        // The listener is bound on the SERVER thread, so the address has to
        // come back over a channel.
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                let addr = listener.local_addr().expect("addr");
                let _ = addr_tx.send(addr);

                // The shutdown signal arrives from a non-async thread, so it
                // is polled rather than awaited: `recv` on a channel that has
                // no runtime dependency. Wrapping it in a `select!` arm via
                // `tokio::task::spawn_blocking` would be more machinery than
                // the question needs — a short poll interval is enough, and
                // a test fixture being up to 50 ms slow to exit is not a
                // correctness problem.
                let mut shutdown = false;
                while !shutdown {
                    let accept = tokio::time::timeout(
                        Duration::from_millis(50),
                        listener.accept(),
                    )
                    .await;
                    let (mut sock, _) = match accept {
                        Ok(Ok(pair)) => pair,
                        Ok(Err(_)) => break,
                        // A timeout is not a failure: it is the one place to
                        // notice the test dropped us. The signal is a DROPPED
                        // sender, not a sent value — `Drop for Server` drops
                        // it — so `Disconnected` is the condition to match.
                        // Reading only `Ok` here would mean the loop never
                        // notices and `join` hangs forever, which is exactly
                        // the bug this fixture had before it stopped counting
                        // connections.
                        Err(_) => {
                            use std::sync::mpsc::TryRecvError;
                            shutdown = !matches!(
                                shutdown_rx.try_recv(),
                                Err(TryRecvError::Empty)
                            );
                            continue;
                        }
                    };

                    // Read to the end of the headers. A POST with no
                    // Content-Length still ends at the blank line, and a GET
                    // has no body at all, so stopping there covers both — and
                    // waiting for a body would hang on a keep-alive.
                    let mut buf = vec![0u8; 8192];
                    let mut got: Vec<u8> = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                got.extend_from_slice(&buf[..n]);
                                if got.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let request = String::from_utf8_lossy(&got).to_string();

                    let reason = match status {
                        200 => "OK",
                        429 => "Too Many Requests",
                        _ => "Error",
                    };

                    if echo {
                        // The request itself, as one SSE event, so the caller
                        // can assert on the headers and body it sent.
                        let body = format!(
                            "event: echo\ndata: {}\n\n",
                            serde_json::Value::String(request)
                        );
                        let head = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(body.as_bytes()).await;
                        let _ = sock.flush().await;
                        let _ = sock.shutdown().await;
                        continue;
                    }

                    // No Content-Length on the streamed path: the response
                    // ends when the socket closes, which is how a real SSE
                    // endpoint frames itself.
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    for c in &chunks {
                        let _ = sock.write_all(c.as_bytes()).await;
                        let _ = sock.flush().await;
                        // Let the reader observe each chunk as its own
                        // arrival rather than one coalesced read.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let _ = sock.shutdown().await;
                }
            });
        });

        let addr = addr_rx.recv().expect("the server bound an address");
        Server {
            url: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    /// A bridge over a scratch home, the way `bridgeops`'s own tests build
    /// one. The stream ops need it for `block_on` and `spawn`.
    fn bridge() -> (
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        std::sync::Arc<crate::bridge::Bridge>,
    ) {
        let (home, guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("empty config");
        let b = crate::bridge::Bridge::new().expect("bridge boots from a readable config");
        (home, guard, b)
    }

    /// Open a stream and return its id, asserting the status on the way.
    fn open_id(
        b: &crate::bridge::Bridge,
        url: &str,
        want_status: u64,
        headers: Option<&str>,
    ) -> u64 {
        let opened = open(b, url, "{}", "application/json", headers)
            .unwrap_or_else(|e| panic!("open failed: {e}"));
        let v: Value = serde_json::from_str(&opened).expect("open answered JSON");
        assert_eq!(
            v["status"].as_u64(),
            Some(want_status),
            "the status arrives with open, before any body"
        );
        v["id"].as_u64().expect("an id")
    }

    /// Pull events until the stream ends, returning each `event` payload.
    fn drain(b: &crate::bridge::Bridge, id: u64) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let raw = next(b, id).expect("next");
            let v: Value = serde_json::from_str(&raw).expect("next answered JSON");
            if v["done"].as_bool().unwrap_or(false) {
                return out;
            }
            if let Some(e) = v["error"].as_str() {
                panic!("stream errored: {e}");
            }
            out.push(v["event"].as_str().unwrap_or("").to_string());
        }
    }

    /// Events arrive ONE AT A TIME, not all at once.
    ///
    /// This is the assertion `http_post` cannot satisfy: it awaits
    /// `resp.text()`, so a driver built on it would see this whole body as a
    /// single string after the server closed. Pulling three events separately
    /// is what proves the transport is incremental.
    #[test]
    fn events_are_pulled_one_at_a_time() {
        let (_h, _g, b) = bridge();
        let before = open_streams();
        let s = server(
            200,
            "text/event-stream",
            vec![
                "event: message_start\ndata: {\"n\":1}\n\n".to_string(),
                "event: message_delta\ndata: {\"n\":2}\n\n".to_string(),
                "event: message_stop\ndata: {\"n\":3}\n\n".to_string(),
            ],
        );

        let id = open_id(&b, &format!("{}/v1/messages", s.url()), 200, None);
        assert_eq!(
            drain(&b, id),
            vec![r#"{"n":1}"#, r#"{"n":2}"#, r#"{"n":3}"#],
            "three SSE events, each pulled separately"
        );
        assert_eq!(
            open_streams(),
            before,
            "a stream that ended frees its own handle"
        );
    }

    /// A `ping` between two real events does not produce a spurious event, and
    /// does not end the stream.
    ///
    /// Anthropic sends pings as keepalives while it thinks. A driver that
    /// treated an event it did not recognize as a terminator would cut long
    /// turns off early — which is the failure mode that looks exactly like a
    /// model that stopped mid-sentence.
    #[test]
    fn a_ping_does_not_end_the_stream() {
        let (_h, _g, b) = bridge();
        let s = server(
            200,
            "text/event-stream",
            vec![
                "data: {\"n\":1}\n\n".to_string(),
                "event: ping\ndata: {\"type\":\"ping\"}\n\n".to_string(),
                "data: {\"n\":2}\n\n".to_string(),
            ],
        );
        let id = open_id(&b, s.url(), 200, None);
        // The ping IS delivered as an event — this module does SSE framing,
        // not Anthropic's protocol, and swallowing a payload would be the
        // driver's decision to make. What matters is that the stream did not
        // end at it.
        let got = drain(&b, id);
        assert_eq!(got.len(), 3, "all three frames arrived: {got:?}");
        assert!(
            got[2].contains("\"n\":2"),
            "and the event after the ping still arrived: {got:?}"
        );
    }

    /// The status is known BEFORE the body is read, which is what lets a
    /// driver classify 429 and 5xx without consuming a stream — and therefore
    /// what lets a plugin route take part in failover.
    #[test]
    fn a_non_2xx_status_arrives_first_and_still_delivers_its_body() {
        let (_h, _g, b) = bridge();
        let before = open_streams();
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        let s = server(429, "application/json", vec![body.to_string()]);

        let id = open_id(&b, s.url(), 429, None);
        // The error body still arrives — as ONE event, then done — so the
        // error path and the success path are the same read loop. A driver
        // does not need a second code path for "the server answered with an
        // error envelope".
        let got = drain(&b, id);
        assert_eq!(
            got,
            vec![body],
            "the endpoint's own error envelope reaches the driver intact"
        );
        assert_eq!(open_streams(), before, "and the handle is freed");
    }

    /// An unreachable host is an `Err` from `open` — not a hang, and not a
    /// stream that opens successfully and then fails on the first read.
    #[test]
    fn a_connection_failure_is_reported_by_open() {
        let (_h, _g, b) = bridge();
        let before = open_streams();
        // Port 1 on loopback: nothing listens, and the refusal is immediate.
        let err = open(
            &b,
            "http://127.0.0.1:1/v1/messages",
            "{}",
            "application/json",
            None,
        )
        .expect_err("cannot connect");
        assert!(!err.is_empty(), "and it says something: {err}");
        assert_eq!(
            open_streams(),
            before,
            "a failed open registers nothing to leak"
        );
    }

    /// Headers reach the wire. A driver that cannot set them cannot
    /// authenticate against anything that is not bearer-tokened, which is the
    /// whole reason the argument exists (Anthropic wants `x-api-key` plus a
    /// dated `anthropic-version`).
    #[test]
    fn custom_headers_reach_the_wire() {
        let (_h, _g, b) = bridge();
        let s = echo_server();

        let headers = r#"{"x-api-key":"sk-secret","anthropic-version":"2023-06-01"}"#;
        let id = open_id(&b, &format!("{}/v1/messages", s.url()), 200, Some(headers));
        let got = drain(&b, id);
        let echoed = got.join("\n").to_lowercase();
        assert!(
            echoed.contains("x-api-key: sk-secret"),
            "the credential must cross the socket, got: {echoed}"
        );
        assert!(
            echoed.contains("anthropic-version: 2023-06-01"),
            "and the dated version header with it, got: {echoed}"
        );
    }

    /// A malformed headers argument is refused with a message about headers,
    /// before anything is connected. Failing at the socket would leave the
    /// driver reporting a network error for what is a config typo.
    #[test]
    fn bad_headers_are_refused_before_connecting() {
        let (_h, _g, b) = bridge();
        let before = open_streams();
        let err = open(
            &b,
            "http://127.0.0.1:1/x",
            "{}",
            "application/json",
            Some("{not json"),
        )
        .expect_err("not a headers object");
        assert!(
            err.contains("header"),
            "the refusal must be about the headers: {err}"
        );
        assert_eq!(open_streams(), before);
    }

    /// `close` frees the handle and is idempotent.
    ///
    /// A driver that returns early on an error is the common shape, so closing
    /// twice — or closing after the stream ended on its own — must not be an
    /// error: what a driver needs guaranteed is that nothing is left running,
    /// not that it can tell whether something was.
    #[test]
    fn close_frees_the_handle_and_can_be_repeated() {
        let (_h, _g, b) = bridge();
        let before = open_streams();
        let s = server(200, "text/event-stream", vec![]); // never sends; the
                                                          // point is that close
                                                          // abandons it
        let id = open_id(&b, s.url(), 200, None);
        assert_eq!(open_streams(), before + 1, "open registers exactly one");

        close(id).expect("close");
        assert_eq!(open_streams(), before, "and frees it");
        close(id).expect("closing twice is not an error");

        let err = next(&b, id).expect_err("a closed stream cannot be read");
        assert!(
            err.contains(&id.to_string()),
            "the refusal names the id it was asked about: {err}"
        );
    }

    /// A stream that ends on its own leaves nothing behind even if the caller
    /// never closes it — the case a driver hits when it returns early on a
    /// transport error.
    #[test]
    fn a_stream_that_ends_frees_itself_without_a_close() {
        let (_h, _g, b) = bridge();
        let before = open_streams();
        let s = server(200, "text/event-stream", vec!["data: {}\n\n".to_string()]);
        let id = open_id(&b, s.url(), 200, None);
        let _ = drain(&b, id);
        assert_eq!(
            open_streams(),
            before,
            "a terminal `done` frees the handle; nothing waits for close"
        );
    }

    /// `close_all` is the plugin-unload path: a driver that returned without
    /// closing must not leave a task and a socket alive past the library that
    /// owns it.
    #[test]
    fn close_all_sweeps_whatever_is_left() {
        let (_h, _g, b) = bridge();
        let before = open_streams();
        let s = server(200, "text/event-stream", vec![]);

        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(open_id(&b, s.url(), 200, None));
        }
        assert_eq!(open_streams(), before + 3, "three live streams");
        assert!(close_all() >= 3, "all of them swept");
        assert_eq!(open_streams(), 0);
        for id in ids {
            assert!(
                next(&b, id).is_err(),
                "and none of them is readable any more"
            );
        }
    }

    /// A stream id that is not a positive integer is refused, and the refusal
    /// says what an id has to be.
    ///
    /// Tested through `bridgeops::stream_id` — the ONE parse both `next` and
    /// `close` go through — rather than a copy of it, so a plugin gets the same
    /// answer from either op and this test cannot keep passing against a
    /// duplicate while the real function is wrong.
    #[test]
    fn a_stream_id_must_be_a_positive_integer() {
        for bad in ["abc", "", "-1", "1.5", "  ", "99999999999999999999999"] {
            let err = crate::bridgeops::stream_id(bad)
                .expect_err(&format!("{bad:?} is not a stream id and must be refused"))
                .to_string();
            assert!(
                err.contains("positive integer"),
                "{bad:?}: the refusal must say what an id is, got {err}"
            );
        }
        // And a real one parses, whitespace and all.
        assert_eq!(crate::bridgeops::stream_id(" 42 ").expect("parses"), 42);
    }

    /// A stream that ends without a terminating blank line still yields its
    /// last event.
    ///
    /// `crate::sse::tail_safe` is what does that, and this is the test that
    /// keeps this module on it: the same truncation cost every reply its tail
    /// once already (2026-09-08), and a second SSE reader is a second chance
    /// to reintroduce it.
    #[test]
    fn a_final_event_without_its_blank_line_is_not_dropped() {
        let (_h, _g, b) = bridge();
        let s = server(
            200,
            "text/event-stream",
            // No trailing blank line, and no second chunk to flush it: the
            // server just closes.
            vec!["data: {\"tail\":\"kept\"}\n".to_string()],
        );
        let id = open_id(&b, s.url(), 200, None);
        assert_eq!(
            drain(&b, id),
            vec![r#"{"tail":"kept"}"#],
            "the hung final event must be promoted, not lost"
        );
    }

    /// A stream is bounded. `http_post` caps a single buffered body at 1 MB;
    /// a stream can legitimately be larger because it is many events, so the
    /// bound is per-event and per-stream — and exceeding it is an ERROR the
    /// driver can report, not a process that grows until it dies.
    #[test]
    fn an_oversized_event_is_an_error_not_a_crash() {
        let (_h, _g, b) = bridge();
        let huge = "x".repeat(EVENT_CAP + 1024);
        let s = server(200, "text/event-stream", vec![format!("data: {huge}\n\n")]);
        let id = open_id(&b, s.url(), 200, None);

        let mut saw_error = false;
        loop {
            let v: Value =
                serde_json::from_str(&next(&b, id).expect("next")).expect("next answered JSON");
            if v["done"].as_bool().unwrap_or(false) {
                break;
            }
            if v["error"].as_str().is_some() {
                saw_error = true;
                break;
            }
        }
        assert!(
            saw_error,
            "an event over the cap must be reported, not buffered into memory"
        );
        assert_eq!(open_streams(), 0, "and the handle is freed on the way out");
    }

    /// `[DONE]` is passed through rather than swallowed. It is an OpenAI
    /// convention Anthropic never sends, and this module does SSE framing, not
    /// protocol: a driver pointed at an OpenAI-shaped gateway needs to see it,
    /// and hiding it here would make that gateway's end-of-stream marker
    /// unreachable.
    #[test]
    fn an_openai_done_marker_is_passed_through_for_the_driver_to_see() {
        let (_h, _g, b) = bridge();
        let s = server(
            200,
            "text/event-stream",
            vec![
                "data: {\"n\":1}\n\n".to_string(),
                "data: [DONE]\n\n".to_string(),
            ],
        );
        let id = open_id(&b, s.url(), 200, None);
        assert_eq!(
            drain(&b, id),
            vec![r#"{"n":1}"#, "[DONE]"],
            "framing, not protocol: the marker arrives as an event"
        );
    }
}
