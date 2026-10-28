//! JSON-RPC 2.0, one message per line, in BOTH directions.
//!
//! The gateway and a process plugin each run one [`Peer`] over the
//! plugin's stdin/stdout. Either side may call the other at any time —
//! the gateway delivers an outbound message to the plugin while the
//! plugin is in the middle of asking the gateway to resolve a sender —
//! so this is not the client the MCP code has (`veles_core::mcp`, which
//! only sends requests and drops anything the server asks it).
//!
//! # What the reader does with each line
//!
//! - a **request** (`method` and `id`) runs the handler on its own task
//!   and answers with the same `id`. Requests run concurrently: one slow
//!   request must not hold up the next, and a handler that calls back
//!   into the other side (which needs this reader to see the answer)
//!   must not deadlock it.
//! - a **notification** (`method`, no `id`) goes to ONE queue that a
//!   single task drains in arrival order. Events are notifications, and
//!   an IRC line, its JOIN and its PART mean something only in order.
//! - a **response** (`id` and `result` or `error`) completes the call
//!   that is waiting for it.
//!
//! # Limits, because the other side can be broken
//!
//! A plugin is owner-installed code, but it can be buggy, and a bug on
//! the far end of a pipe must not take the gateway with it:
//!
//! - a line longer than [`Options::max_line`] closes the connection (an
//!   unterminated line would otherwise grow the buffer until the process
//!   dies — the irc-proto `LineCodec` lesson, vendor/irc-proto);
//! - more than [`Options::max_inflight`] concurrent requests from the
//!   other side are answered [`RpcError::BUSY`] at once, never queued
//!   (queuing would make the reader wait, and a waiting reader cannot
//!   deliver the answers those handlers may be waiting for);
//! - more than [`Options::max_queued_notifications`] unprocessed
//!   notifications close the connection, for the same reason.
//!
//! Every call that is still waiting when the connection ends gets
//! [`RpcError::CLOSED`]; a call made after that gets it at once.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// The connection is gone (or went away while the call waited).
    pub const CLOSED: i64 = -32000;
    /// No answer within the caller's deadline.
    pub const TIMEOUT: i64 = -32001;
    /// The other side has too many requests in flight already.
    pub const BUSY: i64 = -32002;
    /// The request was understood and refused (an ACL, an undeclared
    /// secret). Reserved here so both sides agree on the number.
    pub const REFUSED: i64 = -32003;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(Self::METHOD_NOT_FOUND, format!("no method {method}"))
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(Self::INVALID_PARAMS, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Self::INTERNAL, message)
    }
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(Self::REFUSED, message)
    }
    fn closed(why: &str) -> Self {
        Self::new(Self::CLOSED, format!("connection closed: {why}"))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// What answers the other side's requests and notifications.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    /// A request. The answer goes back with the request's id.
    async fn request(&self, peer: &Peer, method: &str, params: Value) -> Result<Value, RpcError>;
    /// A notification. Called one at a time, in arrival order.
    async fn notification(&self, peer: &Peer, method: &str, params: Value);
}

/// The limits of one connection (see the module docs for why each one).
#[derive(Debug, Clone)]
pub struct Options {
    pub max_line: usize,
    pub max_inflight: usize,
    pub max_queued_notifications: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_line: 16 * 1024 * 1024,
            max_inflight: 256,
            max_queued_notifications: 65_536,
        }
    }
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The other side closed its end.
    Eof,
    /// Reading failed.
    Io(String),
    /// A line over [`Options::max_line`] bytes.
    LineTooLong(usize),
    /// More than [`Options::max_queued_notifications`] waiting.
    NotificationBacklog(usize),
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloseReason::Eof => write!(f, "the other side closed the connection"),
            CloseReason::Io(e) => write!(f, "read failed: {e}"),
            CloseReason::LineTooLong(n) => write!(f, "a line over {n} bytes"),
            CloseReason::NotificationBacklog(n) => {
                write!(f, "more than {n} notifications waiting")
            }
        }
    }
}

type Waiter = oneshot::Sender<Result<Value, RpcError>>;

struct Calls {
    /// Set once, when the connection ends; a call that sees it fails at
    /// once. It lives under the same lock as `waiting` so a call cannot
    /// register itself after the drain and wait forever.
    closed: Option<String>,
    waiting: HashMap<u64, Waiter>,
}

struct Inner {
    out: mpsc::UnboundedSender<String>,
    calls: Mutex<Calls>,
    next_id: AtomicU64,
}

/// One end of a connection. Cheap to clone; every clone is the same end.
#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

impl Peer {
    /// Call `method` and wait for the answer, however long it takes.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, rx) = self.send_request(method, params)?;
        rx.await
            .unwrap_or_else(|_| Err(RpcError::closed("the reader stopped")))
    }

    /// Call `method`; give up after `limit`. A late answer is dropped.
    pub async fn call_timeout(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, RpcError> {
        let (id, rx) = self.send_request(method, params)?;
        match tokio::time::timeout(limit, rx).await {
            Ok(r) => r.unwrap_or_else(|_| Err(RpcError::closed("the reader stopped"))),
            Err(_) => {
                self.lock().waiting.remove(&id);
                Err(RpcError::new(
                    RpcError::TIMEOUT,
                    format!("{method}: no answer in {} ms", limit.as_millis()),
                ))
            }
        }
    }

    /// Send a notification. Nothing comes back.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        if let Some(why) = &self.lock().closed {
            return Err(RpcError::closed(why));
        }
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_line(&msg)
    }

    /// Has the connection ended?
    pub fn is_closed(&self) -> bool {
        self.lock().closed.is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Calls> {
        self.inner.calls.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(u64, oneshot::Receiver<Result<Value, RpcError>>), RpcError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut calls = self.lock();
            if let Some(why) = &calls.closed {
                return Err(RpcError::closed(why));
            }
            calls.waiting.insert(id, tx);
        }
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.send_line(&msg) {
            self.lock().waiting.remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    fn send_line(&self, msg: &Value) -> Result<(), RpcError> {
        // serde_json escapes every control character inside strings, so
        // a serialized message never contains a raw newline: one line is
        // one message.
        let line = serde_json::to_string(msg).map_err(|e| RpcError::internal(e.to_string()))?;
        self.inner
            .out
            .send(line)
            .map_err(|_| RpcError::closed("the writer stopped"))
    }

    fn answer(&self, id: Value, result: Result<Value, RpcError>) {
        let msg = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        let _ = self.send_line(&msg);
    }

    /// End the connection for every waiting and future call.
    fn close(&self, why: &str) {
        let waiting = {
            let mut calls = self.lock();
            if calls.closed.is_none() {
                calls.closed = Some(why.to_string());
            }
            std::mem::take(&mut calls.waiting)
        };
        for (_, w) in waiting {
            let _ = w.send(Err(RpcError::closed(why)));
        }
    }

    fn complete(&self, id: u64, result: Result<Value, RpcError>) {
        let waiter = self.lock().waiting.remove(&id);
        match waiter {
            Some(w) => {
                let _ = w.send(result);
            }
            // A call that timed out, or an id we never sent.
            None => tracing::debug!("jsonrpc: an answer for id {id}, which nothing waits for"),
        }
    }
}

/// Start a connection over `reader`/`writer`. Returns this end, and a
/// task that finishes when the connection ends, saying why.
pub fn connect<R, W, H>(
    reader: R,
    writer: W,
    handler: Arc<H>,
    opts: Options,
) -> (Peer, tokio::task::JoinHandle<CloseReason>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    H: Handler + ?Sized,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let peer = Peer {
        inner: Arc::new(Inner {
            out: out_tx,
            calls: Mutex::new(Calls {
                closed: None,
                waiting: HashMap::new(),
            }),
            next_id: AtomicU64::new(1),
        }),
    };
    tokio::spawn(write_loop(writer, out_rx, peer.clone()));
    let done = tokio::spawn(read_loop(reader, peer.clone(), handler, opts));
    (peer, done)
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<String>,
    peer: Peer,
) {
    while let Some(line) = rx.recv().await {
        let res = async {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await
        }
        .await;
        if let Err(e) = res {
            // Nobody will ever read what we write: every waiting call
            // would otherwise wait for the reader's EOF, which a peer
            // that stopped reading but kept its end open never sends.
            peer.close(&format!("write failed: {e}"));
            return;
        }
    }
}

enum Read {
    Eof,
    Line,
    TooLong,
}

/// One line into `buf` (without the newline), never holding more than
/// `max` bytes of it.
async fn read_line_capped<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Read> {
    buf.clear();
    loop {
        let avail = r.fill_buf().await?;
        if avail.is_empty() {
            // EOF. A last line without its newline still counts.
            return Ok(if buf.is_empty() {
                Read::Eof
            } else {
                Read::Line
            });
        }
        match avail.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if buf.len() + i > max {
                    return Ok(Read::TooLong);
                }
                buf.extend_from_slice(&avail[..i]);
                r.consume(i + 1);
                return Ok(Read::Line);
            }
            None => {
                let n = avail.len();
                if buf.len() + n > max {
                    return Ok(Read::TooLong);
                }
                buf.extend_from_slice(avail);
                r.consume(n);
            }
        }
    }
}

async fn read_loop<R, H>(reader: R, peer: Peer, handler: Arc<H>, opts: Options) -> CloseReason
where
    R: AsyncRead + Unpin + Send + 'static,
    H: Handler + ?Sized,
{
    let mut r = BufReader::new(reader);
    let mut buf = Vec::new();
    let inflight = Arc::new(AtomicUsize::new(0));
    let queued = Arc::new(AtomicUsize::new(0));

    // The one task that runs notifications, in order.
    let (note_tx, mut note_rx) = mpsc::unbounded_channel::<(String, Value)>();
    {
        let (peer, handler, queued) = (peer.clone(), handler.clone(), queued.clone());
        tokio::spawn(async move {
            while let Some((method, params)) = note_rx.recv().await {
                handler.notification(&peer, &method, params).await;
                queued.fetch_sub(1, Ordering::AcqRel);
            }
        });
    }

    let reason = loop {
        match read_line_capped(&mut r, &mut buf, opts.max_line).await {
            Err(e) => break CloseReason::Io(e.to_string()),
            Ok(Read::Eof) => break CloseReason::Eof,
            Ok(Read::TooLong) => break CloseReason::LineTooLong(opts.max_line),
            Ok(Read::Line) => {}
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let msg: Value = match serde_json::from_slice(&buf) {
            Ok(Value::Object(m)) => Value::Object(m),
            Ok(_) => {
                peer.answer(
                    Value::Null,
                    Err(RpcError::new(
                        RpcError::INVALID_REQUEST,
                        "not a JSON object (batches are not supported)",
                    )),
                );
                continue;
            }
            Err(e) => {
                peer.answer(
                    Value::Null,
                    Err(RpcError::new(RpcError::PARSE, e.to_string())),
                );
                continue;
            }
        };
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        match (msg.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                if inflight.load(Ordering::Acquire) >= opts.max_inflight {
                    peer.answer(
                        id,
                        Err(RpcError::new(
                            RpcError::BUSY,
                            format!("{} requests already in flight", opts.max_inflight),
                        )),
                    );
                    continue;
                }
                inflight.fetch_add(1, Ordering::AcqRel);
                let method = method.to_string();
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let (peer, handler, inflight) = (peer.clone(), handler.clone(), inflight.clone());
                tokio::spawn(async move {
                    let res = handler.request(&peer, &method, params).await;
                    peer.answer(id, res);
                    inflight.fetch_sub(1, Ordering::AcqRel);
                });
            }
            (Some(method), None) => {
                if queued.fetch_add(1, Ordering::AcqRel) >= opts.max_queued_notifications {
                    break CloseReason::NotificationBacklog(opts.max_queued_notifications);
                }
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let _ = note_tx.send((method.to_string(), params));
            }
            (None, Some(id)) => {
                let Some(n) = id.as_u64() else {
                    tracing::debug!("jsonrpc: an answer with id {id}, which we never send");
                    continue;
                };
                let res = match msg.get("error") {
                    Some(e) => Err(
                        serde_json::from_value::<RpcError>(e.clone()).unwrap_or_else(|_| {
                            RpcError::internal(format!("malformed error object: {e}"))
                        }),
                    ),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                peer.complete(n, res);
            }
            (None, None) => {
                peer.answer(
                    Value::Null,
                    Err(RpcError::new(
                        RpcError::INVALID_REQUEST,
                        "neither a method nor an id",
                    )),
                );
            }
        }
    };
    peer.close(&reason.to_string());
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Answers `echo` with its params, `add` with a sum, `ask_back` by
    /// calling the OTHER side's `echo` from inside the handler, and
    /// `sleep` after the given milliseconds; records notifications.
    #[derive(Default)]
    struct H {
        notes: Mutex<Vec<Value>>,
        note_delay_ms: AtomicU64,
        hold: AtomicBool,
    }

    #[async_trait]
    impl Handler for H {
        async fn request(
            &self,
            peer: &Peer,
            method: &str,
            params: Value,
        ) -> Result<Value, RpcError> {
            match method {
                "echo" => Ok(params),
                "add" => {
                    let a = params[0]
                        .as_i64()
                        .ok_or_else(|| RpcError::invalid_params("a"))?;
                    let b = params[1]
                        .as_i64()
                        .ok_or_else(|| RpcError::invalid_params("b"))?;
                    Ok(json!(a + b))
                }
                "ask_back" => peer.call("echo", json!({"from": "inside"})).await,
                "sleep" => {
                    let ms = params.as_u64().unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(json!(ms))
                }
                "hold" => {
                    while self.hold.load(Ordering::Acquire) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Ok(Value::Null)
                }
                other => Err(RpcError::method_not_found(other)),
            }
        }
        async fn notification(&self, _peer: &Peer, method: &str, params: Value) {
            let d = self.note_delay_ms.load(Ordering::Acquire);
            if d > 0 {
                // A slow handler for EARLY notifications: a reordering
                // dispatcher would let later ones overtake it.
                let n = params.as_u64().unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(d.saturating_sub(n))).await;
            }
            self.notes
                .lock()
                .unwrap()
                .push(json!({"m": method, "p": params}));
        }
    }

    fn pair(
        opts: Options,
    ) -> (
        Peer,
        Arc<H>,
        Peer,
        Arc<H>,
        tokio::task::JoinHandle<CloseReason>,
    ) {
        let (a_io, b_io) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a_io);
        let (br, bw) = tokio::io::split(b_io);
        let ha = Arc::new(H::default());
        let hb = Arc::new(H::default());
        let (a, _ad) = connect(ar, aw, ha.clone(), opts.clone());
        let (b, bd) = connect(br, bw, hb.clone(), opts);
        (a, ha, b, hb, bd)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calls_go_both_ways_and_nest() {
        let (a, _ha, b, _hb, _bd) = pair(Options::default());
        assert_eq!(a.call("add", json!([2, 3])).await, Ok(json!(5)));
        assert_eq!(b.call("echo", json!({"x": 1})).await, Ok(json!({"x": 1})));
        // B's handler calls A while A waits for B: the reader that must
        // deliver the inner answer is not the one running the handler.
        let r = tokio::time::timeout(Duration::from_secs(5), a.call("ask_back", Value::Null))
            .await
            .expect("a nested call deadlocked");
        assert_eq!(r, Ok(json!({"from": "inside"})));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn errors_come_back_as_errors() {
        let (a, _ha, _b, _hb, _bd) = pair(Options::default());
        let e = a.call("nope", Value::Null).await.unwrap_err();
        assert_eq!(e.code, RpcError::METHOD_NOT_FOUND);
        let e = a.call("add", json!(["x", 1])).await.unwrap_err();
        assert_eq!(e.code, RpcError::INVALID_PARAMS);
    }

    /// Notifications arrive in the order they were sent, even when the
    /// handler takes LONGER for the earlier ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn notifications_keep_their_order() {
        let (a, _ha, _b, hb, _bd) = pair(Options::default());
        hb.note_delay_ms.store(40, Ordering::Release);
        for i in 0..40u64 {
            a.notify("ev", json!(i)).unwrap();
        }
        // A request after them is answered on its own task; wait for the
        // queue instead.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while hb.notes.lock().unwrap().len() < 40 {
            assert!(tokio::time::Instant::now() < deadline, "notifications lost");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let got: Vec<u64> = hb
            .notes
            .lock()
            .unwrap()
            .iter()
            .map(|n| n["p"].as_u64().unwrap())
            .collect();
        assert_eq!(got, (0..40).collect::<Vec<_>>());
    }

    /// When the other side stops sending, a waiting call fails, and so
    /// does every call after it — at once, not after a timeout. The
    /// write half here is a sink that accepts everything, so nothing but
    /// the closed flag can fail those calls: a peer that closed its
    /// stdout but kept reading would otherwise swallow them forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_connection_fails_calls() {
        let (a_io, b_io) = tokio::io::duplex(1 << 16);
        let (ar, _unused_writer) = tokio::io::split(a_io);
        let ha = Arc::new(H::default());
        let (a, ad) = connect(ar, tokio::io::sink(), ha, Options::default());
        let waiting = {
            let a = a.clone();
            tokio::spawn(async move { a.call("echo", json!(1)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(b_io);
        let r = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the waiting call never ended")
            .unwrap();
        assert_eq!(r.unwrap_err().code, RpcError::CLOSED);
        assert_eq!(ad.await.unwrap(), CloseReason::Eof);
        assert!(a.is_closed());
        let r = tokio::time::timeout(Duration::from_secs(1), a.call("echo", json!(2)))
            .await
            .expect("a call after the close waited");
        assert_eq!(r.unwrap_err().code, RpcError::CLOSED);
        assert_eq!(a.notify("ev", json!(1)).unwrap_err().code, RpcError::CLOSED);
    }

    /// The other way round: the far end stopped READING (its stdin is
    /// gone) but never closes what it writes. The failed write is the
    /// only signal, and it must end the waiting calls.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_write_fails_the_waiting_calls() {
        // Reader: a pipe nobody ever writes to or closes.
        let (quiet, _keep_open) = tokio::io::duplex(64);
        // Writer: a pipe whose reading end is already gone.
        let (w_io, gone) = tokio::io::duplex(64);
        drop(gone);
        let (a, _ad) = connect(quiet, w_io, Arc::new(H::default()), Options::default());
        let r = tokio::time::timeout(Duration::from_secs(5), a.call("echo", json!(1)))
            .await
            .expect("the call waited on a dead writer");
        assert_eq!(r.unwrap_err().code, RpcError::CLOSED);
        assert!(a.is_closed());
    }

    /// A line over the cap ends the connection; it is never buffered
    /// whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_oversized_line_closes_the_connection() {
        let (a_io, mut b_io) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a_io);
        let opts = Options {
            max_line: 1024,
            ..Options::default()
        };
        let (a, ad) = connect(ar, aw, Arc::new(H::default()), opts);
        // Exactly the cap is fine…
        let (head, tail) = (
            "{\"jsonrpc\":\"2.0\",\"method\":\"ev\",\"params\":\"",
            "\"}\n",
        );
        let ok = format!(
            "{head}{}{tail}",
            "x".repeat(1024 - head.len() - (tail.len() - 1))
        );
        assert_eq!(ok.len() - 1, 1024);
        b_io.write_all(ok.as_bytes()).await.unwrap();
        // …one byte more, never terminated, is not.
        b_io.write_all(&vec![b'y'; 1025]).await.unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(5), ad)
            .await
            .expect("the reader kept buffering")
            .unwrap();
        assert_eq!(reason, CloseReason::LineTooLong(1024));
        assert!(a.is_closed());
    }

    /// Garbage gets a parse error back and the connection carries on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn garbage_is_answered_and_survived() {
        let (a_io, b_io) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a_io);
        let (br, mut bw) = tokio::io::split(b_io);
        let (_a, _ad) = connect(ar, aw, Arc::new(H::default()), Options::default());
        let mut lines = BufReader::new(br).lines();
        bw.write_all(b"this is not json\n").await.unwrap();
        let l = lines.next_line().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["error"]["code"], json!(RpcError::PARSE));
        assert_eq!(v["id"], Value::Null);
        bw.write_all(b"[1,2]\n").await.unwrap();
        let v: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(v["error"]["code"], json!(RpcError::INVALID_REQUEST));
        // A string id is echoed back as a string.
        bw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"k7\",\"method\":\"add\",\"params\":[40,2]}\n")
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(v["id"], json!("k7"));
        assert_eq!(v["result"], json!(42));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timed_out_call_ignores_its_late_answer() {
        let (a, _ha, _b, _hb, _bd) = pair(Options::default());
        let e = a
            .call_timeout("sleep", json!(300), Duration::from_millis(30))
            .await
            .unwrap_err();
        assert_eq!(e.code, RpcError::TIMEOUT);
        tokio::time::sleep(Duration::from_millis(400)).await;
        // The connection is still good, and the late answer went nowhere.
        assert_eq!(a.call("add", json!([1, 1])).await, Ok(json!(2)));
    }

    /// A call that times out on an answer that never comes leaves
    /// nothing behind: a plugin that stops answering must not grow the
    /// gateway's table of waiting calls one entry per attempt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timed_out_call_is_forgotten() {
        let (a, _ha, _b, hb, _bd) = pair(Options::default());
        hb.hold.store(true, Ordering::Release);
        for _ in 0..3 {
            let e = a
                .call_timeout("hold", Value::Null, Duration::from_millis(20))
                .await
                .unwrap_err();
            assert_eq!(e.code, RpcError::TIMEOUT);
        }
        let left = a.lock().waiting.len();
        assert_eq!(left, 0, "timed-out calls left in the table");
        hb.hold.store(false, Ordering::Release);
    }

    /// A far end that floods notifications faster than they are handled
    /// is cut off at the cap instead of growing the queue without end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_notification_flood_closes_the_connection() {
        let opts = Options {
            max_queued_notifications: 5,
            ..Options::default()
        };
        let (a, _ha, _b, hb, bd) = pair(opts);
        // The first one takes ten seconds; the rest pile up behind it.
        hb.note_delay_ms.store(10_000, Ordering::Release);
        for i in 0..10u64 {
            let _ = a.notify("ev", json!(i));
        }
        let reason = tokio::time::timeout(Duration::from_secs(5), bd)
            .await
            .expect("the backlog grew without a limit")
            .unwrap();
        assert_eq!(reason, CloseReason::NotificationBacklog(5));
    }

    /// Past the in-flight cap a request is refused at once, and the
    /// connection still answers once the held ones finish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_past_the_cap_are_busy_not_queued() {
        let opts = Options {
            max_inflight: 2,
            ..Options::default()
        };
        let (a, _ha, _b, hb, _bd) = pair(opts);
        hb.hold.store(true, Ordering::Release);
        let h1 = tokio::spawn({
            let a = a.clone();
            async move { a.call("hold", Value::Null).await }
        });
        let h2 = tokio::spawn({
            let a = a.clone();
            async move { a.call("hold", Value::Null).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let e = tokio::time::timeout(Duration::from_secs(2), a.call("add", json!([1, 2])))
            .await
            .expect("the third request was queued behind the held ones")
            .unwrap_err();
        assert_eq!(e.code, RpcError::BUSY);
        hb.hold.store(false, Ordering::Release);
        assert_eq!(h1.await.unwrap(), Ok(Value::Null));
        assert_eq!(h2.await.unwrap(), Ok(Value::Null));
        assert_eq!(a.call("add", json!([1, 2])).await, Ok(json!(3)));
    }
}
