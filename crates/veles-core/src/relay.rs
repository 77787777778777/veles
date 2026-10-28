//! Child → gateway event relay.
//!
//! ## The hole this fills
//!
//! `session_tap` is ONE PROCESS-WIDE broadcast channel, and a delegate
//! or `/btw` child is a separate `veles chat -q` process. So a child
//! publishes a full event stream — deltas, reasoning, tool rounds,
//! notes, `turn-started`/`turn-ended` — to a bus inside its own
//! process that has no subscribers, and the gateway, which is where
//! every attached window is listening, hears none of it.
//!
//! The visible shape of that: attach to a running child in the desktop
//! and you get the replay from the store, then silence for the life of
//! the turn. USER 2026-09-18: "I could see its progress but then at
//! some point it stopped streaming and looked hung even though there
//! were new messages coming in."
//!
//! `daeb7be` mitigated it at ROW granularity — an attached stream
//! polls the store when the bus is quiet and sends the gap as
//! `history` frames. You see each round land. You do not see tokens,
//! live reasoning, or tool calls as they happen. This module is the
//! rest: the child POSTs its own events to the gateway, which
//! republishes them on the bus the windows are already watching.
//!
//! ## Why POST and not in-process children
//!
//! The other shape is running children inside the gateway, which would
//! make streaming, attach and cancel free. It also breaks both halves
//! of the isolation the child surface is built on: `VELES_CHILD_DEPTH`
//! is a PROCESS env guard (`delegate.rs`) and the principal boundary
//! is `acl::principal_from_env` read at child boot. Re-expressing both
//! as in-process state is a re-architecture of the subagent security
//! model, not a streaming fix. The POST shape needs neither changed.
//!
//! ## Shape
//!
//! One task, one direction, best effort:
//!
//!   - ONE forwarder task with a sequential send loop, never
//!     concurrent posts — two in flight can arrive out of order, and
//!     the whole point is a faithful replay.
//!   - Best effort. A gateway that is down, a 401, a refused
//!     connection: retry a bounded number of times, then stop
//!     relaying and let the turn finish normally. The store-follow
//!     tick from `daeb7be` stays underneath as the floor, so giving
//!     up degrades to row granularity rather than to nothing.
//!   - Lag is reported, never hidden. A `Lagged(n)` from the
//!     broadcast becomes a `session-lagged` event carrying the count,
//!     the same word the SSE stream already uses for the same thing.
//!   - No loops: only a process with `VELES_RELAY_TO` set relays, and
//!     nothing sets it on the gateway, so a republished event cannot
//!     bounce back.
//!
//! The wire type is `session_tap::SessionEvent` itself, which was
//! already `Serialize + Deserialize` — the relay invents no vocabulary
//! of its own, and cannot drift from the bus it feeds.

use crate::session_tap::SessionEvent;

/// Env var naming the gateway base URL a child should relay to.
/// Set by `delegate::run_child_routed` on the child it spawns; never
/// set on the gateway itself.
pub const RELAY_ENV: &str = "VELES_RELAY_TO";

/// Path the gateway serves the relay endpoint on.
pub const RELAY_PATH: &str = "/api/session-events";

/// How many times one batch may be attempted before the forwarder
/// gives up.
///
/// Bounded rather than infinite: a child whose gateway died must not
/// spend its turn retrying, and the store-follow tick already covers
/// the degraded case at row granularity. But a batch that IS held and
/// re-sent costs nothing on the happy path, and dropping it costs a
/// stretch of a transcript nobody is told about — the failure this
/// module exists to prevent. Found by the release-mode flake check:
/// one transient refusal cost the whole delta batch of a child's turn.
const MAX_BATCH_ATTEMPTS: u32 = 3;

/// Delay before re-sending a refused batch. Small on purpose — this is
/// a blip, not a backoff strategy — and it holds the batch, so the
/// forwarder keeps reading nothing else: order is preserved by never
/// letting a later batch overtake a refused one.
const RETRY_BACKOFF_MS: u64 = 120;

/// Most events in one POST. A turn streaming tokens produces thousands;
/// batching keeps this to a handful of requests without ever holding
/// an event back waiting for company (see `drain_ready`).
const MAX_BATCH: usize = 256;

/// The relay target for this process, if it is a child that was told
/// to relay.
pub fn target() -> Option<String> {
    std::env::var(RELAY_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Take everything already queued on the receiver, up to `MAX_BATCH`.
///
/// Non-blocking on purpose: the caller has one event in hand and asks
/// what else is ready RIGHT NOW. Nothing is ever delayed to fill a
/// batch — a lone token posts immediately, and a burst posts together.
///
/// Returns the events plus any lag the receiver reported while
/// draining, so the caller can render the gap rather than swallow it.
pub fn drain_ready(
    rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
    batch: &mut Vec<SessionEvent>,
) -> u64 {
    let mut lagged = 0u64;
    while batch.len() < MAX_BATCH {
        match rx.try_recv() {
            Ok(ev) => batch.push(ev),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => lagged += n,
            _ => break,
        }
    }
    lagged
}

/// The event that says "this relay dropped `missed` events".
///
/// A gap the client is told about is a rendering problem; a gap it is
/// not told about is a transcript that quietly lies. The SSE stream
/// already spells this `session-lagged`, so the relay does too.
pub fn lag_event(session_id: &str, missed: u64) -> SessionEvent {
    SessionEvent {
        seq: 0,
        session_id: session_id.to_string(),
        kind: "session-lagged".to_string(),
        data: serde_json::json!({ "missed": missed }),
    }
}

/// Start relaying this process's session events to the gateway, if
/// `VELES_RELAY_TO` says to.
///
/// Returns a handle whose `flush` must be awaited before the process
/// exits — `veles chat -q` ends the moment the turn does, and an
/// un-awaited forwarder would be killed holding the last events of the
/// turn, which are the ones that matter most.
pub fn start() -> Option<RelayHandle> {
    let base = target()?;
    let token = crate::authn::desktop_bearer_token();
    let rx = crate::session_tap::subscribe();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    crate::rt::spawn(async move {
        run_forwarder(base, token, rx, stop_rx).await;
        let _ = done_tx.send(());
    });
    Some(RelayHandle {
        stop: Some(stop_tx),
        done: Some(done_rx),
    })
}

/// Keeps the forwarder alive and lets the caller drain it at exit.
pub struct RelayHandle {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl RelayHandle {
    /// Tell the forwarder the turn is over, then wait for it to post
    /// what it still holds.
    ///
    /// Bounded: a gateway that has stopped answering must not keep a
    /// finished child alive. The events it would have carried are in
    /// the store, which the attached window follows anyway.
    pub async fn flush(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(d) = self.done.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), d).await;
        }
    }
}

async fn run_forwarder(
    base: String,
    token: String,
    mut rx: tokio::sync::broadcast::Receiver<SessionEvent>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let url = format!("{}{}", base.trim_end_matches('/'), RELAY_PATH);
    let client = reqwest::Client::new();
    let mut consecutive_failures = 0u32;
    let mut stopping = false;
    loop {
        let mut batch: Vec<SessionEvent> = Vec::new();
        let mut lagged = 0u64;
        if stopping {
            // Final pass: take whatever is queued and go.
            lagged += drain_ready(&mut rx, &mut batch);
            if batch.is_empty() && lagged == 0 {
                return;
            }
        } else {
            tokio::select! {
                biased;
                r = rx.recv() => match r {
                    Ok(ev) => {
                        batch.push(ev);
                        lagged += drain_ready(&mut rx, &mut batch);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => lagged += n,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                _ = &mut stop => {
                    stopping = true;
                    continue;
                }
            }
        }
        if lagged > 0 {
            let sid = batch
                .first()
                .map(|e| e.session_id.clone())
                .unwrap_or_default();
            batch.insert(0, lag_event(&sid, lagged));
        }
        if batch.is_empty() {
            continue;
        }
        // Post the batch, and do NOT move on until it lands. A refused
        // POST used to be counted and dropped, which loses the events
        // in hand — a window watching the child then shows a transcript
        // with a hole in it and no way to know. Re-sending the SAME
        // batch is what keeps the relay's own promise, and holding it
        // is what keeps the order: nothing later can overtake it.
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let req = client
                .post(&url)
                .json(&batch)
                .timeout(std::time::Duration::from_secs(5));
            let req = if token.is_empty() {
                req
            } else {
                req.bearer_auth(&token)
            };
            match req.send().await {
                Ok(r) if r.status().is_success() => {
                    consecutive_failures = 0;
                    break;
                }
                Ok(r) => {
                    tracing::debug!("session relay rejected: {}", r.status());
                }
                Err(e) => {
                    tracing::debug!("session relay failed: {e}");
                }
            }
            if attempt < MAX_BATCH_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(RETRY_BACKOFF_MS)).await;
                continue;
            }
            consecutive_failures += 1;
            break;
        }
        if consecutive_failures >= MAX_BATCH_ATTEMPTS {
            // Give up rather than spend the turn retrying. The store
            // is still being written and the attached window still
            // follows it; this degrades to row granularity, not to
            // nothing.
            tracing::debug!("session relay giving up after {consecutive_failures} refused batches");
            return;
        }
        if stopping {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn ev(sid: &str, kind: &str, seq: u64) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: sid.to_string(),
            kind: kind.to_string(),
            data: json!({}),
        }
    }

    /// The relay only exists for a process that was TOLD to relay.
    /// Nothing sets `VELES_RELAY_TO` on the gateway, and that is the
    /// whole of the no-loop guarantee: a republished event has nowhere
    /// to bounce to.
    #[test]
    fn only_a_process_that_was_told_to_relay_relays() {
        let _g = crate::testenv::lock();
        std::env::remove_var(RELAY_ENV);
        assert!(target().is_none(), "unset means no relay");
        std::env::set_var(RELAY_ENV, "   ");
        assert!(target().is_none(), "blank means no relay, not a blank URL");
        std::env::set_var(RELAY_ENV, "http://127.0.0.1:8642");
        assert_eq!(target().as_deref(), Some("http://127.0.0.1:8642"));
        std::env::remove_var(RELAY_ENV);
    }

    /// A batch is whatever is ALREADY queued — never a delay waiting
    /// for company. A lone token has to go out on its own or live
    /// streaming is not live.
    #[test]
    fn a_batch_takes_what_is_ready_and_keeps_its_order() {
        let mut rx = crate::session_tap::subscribe();
        let sid = "relay-batch-order";
        for i in 0..5 {
            crate::session_tap::publish(sid, "delta", json!({ "text": i.to_string() }));
        }
        let mut batch = Vec::new();
        let lag = drain_ready(&mut rx, &mut batch);
        assert_eq!(lag, 0);
        let mine: Vec<&SessionEvent> = batch.iter().filter(|e| e.session_id == sid).collect();
        assert_eq!(mine.len(), 5, "everything queued came out");
        let texts: Vec<String> = mine
            .iter()
            .map(|e| e.data["text"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            texts,
            vec!["0", "1", "2", "3", "4"],
            "and in the order it was published — a transcript is an ordering"
        );
        // Nothing left, and no blocking.
        let mut again = Vec::new();
        drain_ready(&mut rx, &mut again);
        assert!(
            again.iter().all(|e| e.session_id != sid),
            "drain_ready must not block waiting to fill a batch"
        );
    }

    /// A dropped event the client is never told about is a transcript
    /// that quietly lies. `session-lagged` is the word the SSE stream
    /// already uses for this, so the relay uses it too rather than
    /// inventing a second name for one thing.
    #[test]
    fn a_dropped_run_of_events_is_reported_not_swallowed() {
        let e = lag_event("s1", 17);
        assert_eq!(e.kind, "session-lagged");
        assert_eq!(e.data["missed"], 17);
        assert_eq!(e.session_id, "s1");
    }

    /// Ordering is the contract, so the wire type has to survive the
    /// trip unchanged. `SessionEvent` was already Serialize +
    /// Deserialize, which is why this relay invents no vocabulary.
    #[test]
    fn the_wire_type_round_trips_whole() {
        let batch = vec![ev("s", "delta", 3), ev("s", "reasoning", 4)];
        let wire = serde_json::to_string(&batch).expect("serialize");
        let back: Vec<SessionEvent> = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].kind, "delta");
        assert_eq!(back[1].kind, "reasoning");
        assert_eq!(back[1].seq, 4);
    }

    /// A POST that fails transiently must NOT lose its batch.
    ///
    /// Found by the release-mode flake check, which caught the child
    /// posting seq 1,2,3 then 6 — the delta batch had vanished. The
    /// cause: `run_forwarder` counted a failure and moved on, so the
    /// events it had in hand were gone. One refused connection, one
    /// 502 from a proxy, one gateway restart mid-turn, and the window
    /// watching the child silently loses a stretch of the transcript —
    /// the precise failure this module's header says it refuses to
    /// accept ("a gap the client is told about is a rendering problem;
    /// a gap it is not told about is a transcript that quietly lies").
    /// The header also already promised "retry a bounded number of
    /// times"; the code did not.
    ///
    /// The stub gateway here fails the FIRST POST with 503 and accepts
    /// everything after, which is the shape of a real blip: the relay
    /// must re-send the same batch, in order, and then carry on.
    #[tokio::test]
    async fn a_transient_post_failure_retries_the_same_batch() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let posts = Arc::new(AtomicUsize::new(0));
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let n = posts.clone();
        let sink = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                use std::io::{Read as _, Write as _};
                let Ok(mut s) = stream else { continue };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let mut head_end = None;
                while head_end.is_none() {
                    match s.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => {
                            raw.extend_from_slice(&buf[..k]);
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
                        Ok(k) => body.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                let nth = n.fetch_add(1, Ordering::SeqCst);
                if nth == 0 {
                    // The blip: refuse this one, keep the socket open
                    // only long enough to answer.
                    let _ = s.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                    );
                    let _ = s.flush();
                    continue;
                }
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).to_string());
                let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
                let _ = s.flush();
            }
        });

        let rx = crate::session_tap::subscribe();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        // A session id of its own: the bus is process-wide and other
        // tests publish beside this one.
        let sid = "relay-retry-test";
        crate::session_tap::publish(sid, "delta", json!({ "text": "kept" }));
        let forwarder = crate::rt::spawn(run_forwarder(
            format!("http://{addr}"),
            String::new(),
            rx,
            stop_rx,
        ));
        // Let it fail once, retry, and succeed.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let _ = stop_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), forwarder).await;

        let bodies = seen.lock().unwrap().clone();
        let all = bodies.join("\n");
        assert!(
            all.contains("kept"),
            "the batch refused once must be re-sent, not dropped — \
             one blip cost the whole batch: {all:?} (of {bodies:?})"
        );
        assert!(
            posts.load(Ordering::SeqCst) >= 2,
            "the refusal has to be followed by an attempt"
        );
    }
}
