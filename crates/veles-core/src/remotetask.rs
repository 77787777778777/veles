//! Remote task execution over the cluster A2A link (B4).
//!
//! `delegate_task backend = <node>` runs the child on ANOTHER pool
//! node: the goal travels as an A2A message framed like the other
//! cluster protocols (marker + base64 + sha256 — see memsync/gossip),
//! the peer's a2a_server intercepts it BEFORE any session or turn and
//! runs its own local child (`veles chat -q`, the same runner every
//! delegate uses), answering inline with the child's JSON verdict.
//!
//! The trust boundary is its OWN list, `[cluster] task_peers` (USER,
//! 2026-09-28), empty by default. B4 shipped it on `memsync_peers`, the
//! list for sync offers the operator applies by hand; running a subagent
//! here — a shell, files, the node's keys — is a different grant, so it
//! is off until the operator names the peer. A bearer token alone does
//! not buy compute; neither does being a sync peer. `from_node` travels
//! in the ask but is INFORMATIONAL — unlike a lease ask, which the
//! arbiter additionally binds to the peer's gossip-observed node id (a
//! lease must not be grabbed for another node), remote work names no
//! node-specific resource, so the allowlist is the whole gate.
//!
//! The child runs under the CALLER's principal, carried in the ask
//! (`principal`) exactly as a local delegate child gets it in its env.
//!
//! Why a dedicated marker instead of sending the goal as a normal A2A
//! turn: a turn would run the goal through the PEER'S AGENT (its
//! model, its tools, its prompt) — an LLM conversation about the task,
//! not the task. `backend = <node>` means "run a subagent PROCESS
//! there", which is exactly what the peer's delegate runner does for
//! its own agent; the marker makes the ask a cluster instruction, not
//! a chat message.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The marker every remote-task message starts with.
pub const REMOTE_TASK_MARKER: &str = "[veles remote task]";
/// The reply marker (the child's JSON verdict rides inside).
pub const REMOTE_REPLY_MARKER: &str = "[veles remote task reply]";

pub fn is_remote_task(raw: &str) -> bool {
    raw.trim_start().starts_with(REMOTE_TASK_MARKER)
}

pub fn is_remote_reply(raw: &str) -> bool {
    raw.trim_start().starts_with(REMOTE_REPLY_MARKER)
}

const FENCE_B64: &str = "```b64";

fn b64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(s.trim())?)
}

/// Split "…\n```b64\nsha256:…\n<b64>\n```" into (sha, b64). Shared
/// shape with gossip.rs's fenced_payload (kept local so the two wire
/// protocols stay independently evolvable).
fn fenced_payload(raw: &str, fence: &str) -> Result<(String, String)> {
    let mut in_fence = false;
    let mut sha = String::new();
    let mut b64 = String::new();
    for l in raw.trim().lines() {
        let t = l.trim();
        if !in_fence {
            if t == fence {
                in_fence = true;
            }
            continue;
        }
        if t == "```" {
            break;
        }
        if sha.is_empty() {
            if let Some(s) = t.strip_prefix("sha256:") {
                sha = s.to_string();
                continue;
            }
        }
        b64.push_str(t);
    }
    if !in_fence || sha.is_empty() || b64.is_empty() {
        anyhow::bail!("no {fence} fence with sha256 + payload found");
    }
    Ok((sha, b64))
}

/// The ask, as it travels.
///
/// No depth field on the wire: the recursion guard does not need one.
/// A remote ask can only originate from a `delegate_task` call, which
/// already refuses at depth ≥ 1 (the same env guard, on the sending
/// node); and the child the peer spawns runs through
/// `run_child_routed`, which sets `VELES_CHILD_DEPTH=1` — so the
/// remote child's own tool registry has no delegate_task either. One
/// hop is the end-to-end maximum, enforced at both ends locally; a
/// wire field would be a claim to verify rather than a property that
/// holds by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTaskAsk {
    /// Sending node id — informational; the TRUST is the peer token.
    pub from_node: String,
    /// The goal, verbatim.
    pub goal: String,
    /// Optional context block.
    pub context: String,
    /// Optional route override for the remote child (model name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// The CALLER's principal on the placing node, serialized exactly as
    /// a local child's `VELES_SESSION_PRINCIPAL` carries it. The peer's
    /// child runs under it; `None` means the caller had none (an owner
    /// surface). Before this travelled, a remote child always ran as
    /// owner, so delegating to a peer widened a Trusted caller to
    /// Cap::All there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The wire task id the SENDER minted (C3). The receiver registers
    /// its A2A task under THIS id up front (WORKING) with a live
    /// cancel token threaded into the child's kill loop, and the
    /// sender's cancel watcher POSTs `tasks/cancel` at it. Without it
    /// the sender had no handle to cancel by, and the receiver's record
    /// only existed AFTER the child finished — so `tasks/cancel`
    /// answered "not found" for the whole run and the child burned to
    /// its cap. Sender-minted because only the sender knows when it
    /// wants to stop; the receiver's own id it could not predict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_task_id: Option<String>,
    /// A ticket for exactly this delegation (PLAN-pool-round.md D2/D3):
    /// the tool `delegate_task`, its arguments `{goal, context}`. Without
    /// one the peer runs the child at most at the User level
    /// (`remotetool::delegate_principal`) — a compromised node must not
    /// get a host shell on another node by delegating to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// The placing session (ASK-2). The peer parents the child row it
    /// mints to it (`delegate::remote_parent`), so once the peer's store
    /// replicates here the child nests under its parent in this node's
    /// sidebar and Sessions window. `None` from an older sender: the
    /// child is parented to the pseudo-session `a2a-remote`, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// A marker line, an optional header line, then `json` base64'd inside
/// a sha256 fence — the frame every ask and reply of the cluster call
/// protocols travels in (remote tasks here, remote tool calls in
/// remotetool.rs).
pub(crate) fn fence(marker: &str, header: &str, json: &str) -> String {
    let sha = crate::memory::MemoryStore::content_sha(json);
    let head = if header.is_empty() {
        marker.to_string()
    } else {
        format!("{marker}\n{header}")
    };
    format!(
        "{head}\n{FENCE_B64}\nsha256:{sha}\n{}",
        b64_encode(json.as_bytes())
    )
}

/// The JSON inside a [`fence`], its checksum verified.
pub(crate) fn unfence(raw: &str) -> Result<String> {
    let (sha, b64) = fenced_payload(raw, FENCE_B64)?;
    let json = String::from_utf8(b64_decode(&b64)?)?;
    if crate::memory::MemoryStore::content_sha(&json) != sha {
        anyhow::bail!("checksum mismatch — the payload changed in transit");
    }
    Ok(json)
}

/// Encode the ask (marker + sha-fenced base64 JSON).
pub fn encode_ask(ask: &RemoteTaskAsk) -> Result<String> {
    Ok(fence(
        REMOTE_TASK_MARKER,
        "remote subagent over the cluster link",
        &serde_json::to_string(ask)?,
    ))
}

/// Parse an ask out of an inbound message (sha verified).
pub fn decode_ask(raw: &str) -> Result<RemoteTaskAsk> {
    serde_json::from_str(&unfence(raw)?).context("payload is not a remote task ask")
}

/// Encode the reply: the child's whole verdict JSON, fenced the same
/// way (the verdict can contain arbitrary text; the fence keeps the
/// marker protocol shape uniform).
pub fn encode_reply(verdict: &serde_json::Value) -> Result<String> {
    Ok(fence(
        REMOTE_REPLY_MARKER,
        "",
        &serde_json::to_string(verdict)?,
    ))
}

/// Parse a reply (sha verified). Returns the verdict JSON.
pub fn decode_reply(raw: &str) -> Result<serde_json::Value> {
    serde_json::from_str(&unfence(raw)?).context("payload is not a remote task reply")
}

/// Send one ask to a peer and await its reply (the a2a `call` waits
/// for the completed task — the peer runs the child synchronously
/// inside its A2A handler before answering, bounded by its own child
/// timeout).
///
/// C3: the call races a [`CancelWatch`]. When the watch fires first,
/// the sender POSTs `tasks/cancel` at the peer under the ask's
/// `wire_task_id` (the receiver registered a WORKING task under it,
/// token threaded into the child's kill loop) and returns the same
/// `{"status":"cancelled"}` verdict shape a LOCAL cancelled child
/// gives — one vocabulary for "you stopped it", local or remote. An
/// ask with no `wire_task_id` (an older sender) keeps the plain
/// await: no handle, no watcher, behavior exactly as B4 shipped it.
pub async fn run_on_peer(
    peer_url: &str,
    bearer: Option<&str>,
    ask: &RemoteTaskAsk,
    watch: CancelWatch,
) -> Result<serde_json::Value> {
    let text = encode_ask(ask)?;
    // The peer's child may legitimately run for its full 900s cap; the
    // transport must outlive it, not declare it dead at 180s.
    let client = crate::a2a::A2AClient::with_timeout(std::time::Duration::from_secs(
        crate::delegate::CHILD_TIMEOUT_SECS + 120,
    ));
    let ctx = crate::a2a::new_context_id();
    let Some(wire_id) = ask.wire_task_id.clone().filter(|_| watch.armed()) else {
        return decode_call_result(client.call(peer_url, bearer, &text, &ctx).await?);
    };
    tokio::select! {
        result = client.call(peer_url, bearer, &text, &ctx) => {
            decode_call_result(result?)
        }
        () = watch.wait() => {
            // Tell the peer to kill its child, then answer the way a
            // local cancel answers. The cancel RPC is best-effort: a
            // peer that is unreachable now will still reap the child
            // at its own cap, and the sender's verdict must not hang
            // on a second round trip.
            let notify = crate::a2a::A2AClient::new()
                .cancel_task(peer_url, bearer, &wire_id)
                .await;
            let mut v = serde_json::json!({
                "status": "cancelled",
                "error": "cancelled by request",
            });
            match notify {
                Ok(state) => v["remote_state"] = serde_json::json!(state),
                Err(e) => {
                    v["remote_cancel_note"] =
                        serde_json::json!(format!("peer did not confirm the cancel: {e:#}"));
                }
            }
            Ok(v)
        }
    }
}

/// The reply leg, factored because the watching and non-watching
/// paths decode identically.
fn decode_call_result(result: serde_json::Value) -> Result<serde_json::Value> {
    let reply_text = crate::a2a::extract_text(&result);
    if !is_remote_reply(&reply_text) {
        // A completed task whose text is not our reply marker: the peer
        // answered SOMETHING (refusal, wrong version). Surface it
        // verbatim — truncation would hide the reason.
        let state = result["status"]["state"].as_str().unwrap_or("?");
        let what = format!("peer did not answer a remote task ({state}): {reply_text}");
        if state == crate::a2a::STATE_REJECTED {
            // Rejected before a child was started (its `[cluster]
            // task_peers` does not name this node, it is locked down, or
            // the context hit its turn cap): nothing ran there.
            return Err(anyhow::Error::new(TaskRejected {
                what,
                reason: reply_text,
            }));
        }
        anyhow::bail!(what)
    }
    decode_reply(&reply_text)
}

/// A peer that took the call and rejected the task before starting a
/// child, with no verdict: its `[cluster] task_peers` does not list this
/// node, it is locked down, or this context reached its ping-pong turn
/// cap. It rides the error chain of [`run_on_peer`] like
/// [`crate::a2a::TurnedAway`]: nothing ran there (COMP-1, 0.2.2).
#[derive(Debug)]
pub struct TaskRejected {
    what: String,
    reason: String,
}

impl TaskRejected {
    /// The reason the peer gave, as it sent it (its own text, so a
    /// display bounds it).
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for TaskRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for TaskRejected {}

/// What may stop a remote task from the SENDER's side (C3).
///
/// Two sources, either of which is enough:
///
///   - `token` — the tasks-registry `CancelToken` the batch/background
///     paths already mint (`veles tasks cancel <id>`, the API, and
///     `/subagents cancel` all flip it). Cloning shares the flag.
///   - `turn_session` — the Stop button's registry: a foreground
///     delegate runs INSIDE a turn, and `agent::cancel_turn(session)`
///     is what every surface's Stop calls. The terminal tool polls the
///     same predicate for its own long commands; this is the remote
///     child's twin of that.
///
/// An unarmed watch (neither source) never fires — `run_on_peer`
/// skips the select entirely, which keeps the no-handle shape honest.
#[derive(Default, Clone)]
pub struct CancelWatch {
    token: Option<crate::providers::CancelToken>,
    turn_session: Option<String>,
}

impl CancelWatch {
    /// Registry-token source (batch/background delegate paths).
    pub fn token(t: crate::providers::CancelToken) -> Self {
        Self {
            token: Some(t),
            turn_session: None,
        }
    }
    /// Turn source (the foreground delegate: Stop cancels the turn the
    /// tool call is running in).
    pub fn turn(session_id: &str) -> Self {
        Self {
            token: None,
            turn_session: Some(session_id.to_string()),
        }
    }
    /// Both sources at once (a batch member inside a live turn: the
    /// registry row AND the Stop button may each stop it).
    pub fn both(t: crate::providers::CancelToken, session_id: &str) -> Self {
        Self {
            token: Some(t),
            turn_session: Some(session_id.to_string()),
        }
    }
    pub fn armed(&self) -> bool {
        self.token.is_some() || self.turn_session.is_some()
    }
    fn fired(&self) -> bool {
        self.token.as_ref().is_some_and(|t| t.is_cancelled())
            || self
                .turn_session
                .as_ref()
                .is_some_and(|s| crate::agent::turn_cancelled(s))
    }
    /// Resolve the moment a source fires. 250ms poll: the local child's
    /// kill loop polls its token every 100ms, and a remote cancel is a
    /// human-timescale event — sub-second notice with no wake plumbing
    /// through two registries.
    pub(crate) async fn wait(&self) {
        loop {
            if self.fired() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> RemoteTaskAsk {
        RemoteTaskAsk {
            from_node: "aaaa".into(),
            goal: "count the GPUs {ohio} — mind the braces".into(),
            context: "".into(),
            model: Some("deepseek-v4-flash-0731".into()),
            provider: None,
            reasoning: None,
            principal: None,
            wire_task_id: None,
            ticket: None,
            parent_session: None,
        }
    }

    /// Wire round trip, including the checksum refusal for tampering.
    #[test]
    fn ask_round_trips_and_detects_tampering() {
        let enc = encode_ask(&ask()).unwrap();
        assert!(is_remote_task(&enc));
        assert!(!is_remote_reply(&enc));
        let dec = decode_ask(&enc).unwrap();
        assert_eq!(dec.goal, "count the GPUs {ohio} — mind the braces");
        assert_eq!(dec.model.as_deref(), Some("deepseek-v4-flash-0731"));

        // Tamper: same fence, same sha line, DIFFERENT base64 payload
        // (decode → change the goal → re-encode; the stale sha must
        // refuse what the bytes actually say).
        let (sha, b64) = {
            let mut lines = enc.lines();
            let mut fence_at = None;
            for (i, l) in lines.by_ref().enumerate() {
                if l.trim() == FENCE_B64 {
                    fence_at = Some(i);
                    break;
                }
            }
            let _ = fence_at;
            fenced_payload(&enc, FENCE_B64).unwrap()
        };
        let mut json: serde_json::Value =
            serde_json::from_slice(&b64_decode(&b64).unwrap()).unwrap();
        json["goal"] = "rewritten in transit".into();
        let forged = format!(
            "{REMOTE_TASK_MARKER}\nremote subagent over the cluster link\n\
             {FENCE_B64}\nsha256:{sha}\n{}",
            b64_encode(serde_json::to_string(&json).unwrap().as_bytes())
        );
        let err = decode_ask(&forged).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err:#}");
    }

    #[test]
    fn reply_round_trips() {
        let verdict = serde_json::json!({
            "status": "ok",
            "summary": "ran on b, 3 files written",
            "remote_node": "bbbb",
            "child_session": "a2a_remote_x"
        });
        let enc = encode_reply(&verdict).unwrap();
        assert!(is_remote_reply(&enc));
        assert!(!is_remote_task(&enc));
        let dec = decode_reply(&enc).unwrap();
        assert_eq!(dec["summary"], "ran on b, 3 files written");
    }

    /// A missing fence is an error, not an empty ask.
    #[test]
    fn a_marker_without_a_fence_is_refused() {
        assert!(decode_ask(REMOTE_TASK_MARKER).is_err());
        assert!(decode_ask(&format!("{REMOTE_TASK_MARKER}\nplain text, no fence")).is_err());
    }

    /// A peer rejects a task for one of three reasons (its task_peers,
    /// its lockdown, the ping-pong cap), and the route cache recorded
    /// every one as "task_peers does not name this node", which `nodes`
    /// then showed. The reason recorded is the one the peer gave, on
    /// one line and bounded, whatever the peer sent.
    #[test]
    fn a_rejected_task_is_recorded_with_the_peers_own_reason() {
        let rejected = |reason: &str| {
            let task = crate::a2a::build_task("t", "c", crate::a2a::STATE_REJECTED, reason);
            let e = decode_call_result(task).expect_err("a rejected task is an error");
            match crate::routeprobe::learned_from(&e) {
                Some(crate::routeprobe::Probe::Refused(why)) => why,
                other => panic!("not a refusal: {other:?}"),
            }
        };
        let lockdown = rejected("[lockdown] veles is locked down by the owner");
        assert!(
            lockdown.contains("[lockdown] veles is locked down"),
            "{lockdown}"
        );
        assert!(!lockdown.contains("task_peers"), "{lockdown}");
        let cap = rejected("turn limit for this context exceeded — tasks/cancel resets it");
        assert!(
            cap.contains("turn limit for this context exceeded"),
            "{cap}"
        );
        let peers = rejected("this node does not take tasks from aaaa ([cluster] task_peers)");
        assert!(peers.contains("does not take tasks from aaaa"), "{peers}");
        let noisy = rejected(&format!(
            "line one\n\x1b[31mline two\x07 {}",
            "x".repeat(2000)
        ));
        assert!(
            !noisy.contains('\n') && !noisy.contains('\x1b'),
            "{noisy:?}"
        );
        assert!(noisy.contains("line one [31mline two"), "{noisy}");
        assert!(noisy.chars().count() < 400, "{}", noisy.chars().count());
    }
}

#[cfg(test)]
mod watch_pins {
    use super::*;
    use crate::providers::CancelToken;

    /// The turn source of the watch reads agent::turn_cancelled, which
    /// reads LIVE_TURNS. cancel_turn() REMOVES the session's tokens
    /// before flipping them — so does turn_cancelled still see the
    /// cancel a moment later? This pin settles it empirically rather
    /// than the watcher being built on an assumption about another
    /// module's registry lifecycle.
    #[test]
    fn turn_source_sees_a_cancel_turn_flip() {
        let sid = "watch-turn-pin-session";
        let tok = CancelToken::new();
        crate::agent::register_turn_for_test(sid, tok);
        let watch = CancelWatch::turn(sid);
        assert!(watch.armed());
        assert!(!watch.fired(), "nothing cancelled yet");
        // The Stop button's path:
        assert!(crate::agent::cancel_turn(sid), "the turn was registered");
        assert!(
            watch.fired(),
            "turn_cancelled must still read the flip after cancel_turn removed the entry"
        );
        crate::agent::clear_turn_for_test(sid);
    }

    /// An unarmed watch never fires and reports not-armed — run_on_peer
    /// skips its select for exactly this shape.
    #[test]
    fn an_unarmed_watch_never_fires() {
        let w = CancelWatch::default();
        assert!(!w.armed());
        assert!(!w.fired());
    }

    /// The token source: a registry flip is seen by the watch.
    #[test]
    fn token_source_sees_a_registry_flip() {
        let tok = CancelToken::new();
        let w = CancelWatch::token(tok.clone());
        assert!(w.armed());
        assert!(!w.fired());
        tok.cancel();
        assert!(w.fired());
    }

    /// `both` fires on EITHER source (a batch member inside a live
    /// turn: the registry row and the Stop button each stop it).
    #[test]
    fn both_sources_fire_independently() {
        let sid = "watch-both-pin-session";
        let tok = CancelToken::new();
        let turn_tok = CancelToken::new();
        crate::agent::register_turn_for_test(sid, turn_tok);
        let w = CancelWatch::both(tok.clone(), sid);
        assert!(!w.fired());
        tok.cancel(); // registry source
        assert!(w.fired(), "the registry token fires it");
        crate::agent::clear_turn_for_test(sid);
    }
    /// The SENDER half: run_on_peer races the call against the watch.
    /// A peer that stalls on message/send (the child still running) is
    /// sent a tasks/cancel under the ask's wire_task_id the moment the
    /// watch fires, and run_on_peer returns the same
    /// {"status":"cancelled"} verdict a local cancelled child gives —
    /// not a hang, not an error. A raw loopback server stands in for
    /// the peer: it never answers the send, and records the cancel.
    #[tokio::test]
    async fn run_on_peer_cancels_a_stalled_peer_and_reports_cancelled() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let saw_cancel = Arc::new(std::sync::Mutex::new(None::<String>));
        let sc = saw_cancel.clone();

        crate::rt::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let sc = sc.clone();
                crate::rt::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let mut need: Option<usize> = None;
                    loop {
                        let n = match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&chunk[..n]);
                        if need.is_none() {
                            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&raw[..end + 4]).to_lowercase();
                                let len = head
                                    .split("content-length:")
                                    .nth(1)
                                    .and_then(|t| t.split(['\r', '\n']).next())
                                    .and_then(|t| t.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                need = Some(end + 4 + len);
                            }
                        }
                        if matches!(need, Some(t) if raw.len() >= t) {
                            break;
                        }
                    }
                    let body = String::from_utf8_lossy(&raw);
                    let method = serde_json::from_str::<serde_json::Value>(
                        body.split("\r\n\r\n").nth(1).unwrap_or("{}"),
                    )
                    .ok();
                    let m = method
                        .as_ref()
                        .and_then(|v| v["method"].as_str())
                        .unwrap_or("");
                    if m == "tasks/cancel" {
                        let tid = method
                            .as_ref()
                            .and_then(|v| v["params"]["id"].as_str())
                            .unwrap_or("")
                            .to_string();
                        *sc.lock().unwrap() = Some(tid);
                        let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"task":{"status":{"state":"TASK_STATE_CANCELED"}}}}"#;
                        let out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp.len(),
                            resp
                        );
                        let _ = sock.write_all(out.as_bytes()).await;
                        let _ = sock.flush().await;
                    }
                    // message/send: NEVER answer — the child is still
                    // running on the real peer; the watch must beat it.
                    // HOLD the socket open (a dropped sock would close
                    // the connection and reqwest would error before the
                    // watch fires — that is the fake misbehaving, not
                    // the cancel path). The task is killed when the
                    // select! drops the call future and the test ends.
                    if m == "message/send" {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        }
                    }
                });
            }
        });

        let mut ask = RemoteTaskAsk {
            from_node: "aaaa".into(),
            goal: "a long job".into(),
            context: String::new(),
            model: None,
            provider: None,
            reasoning: None,
            principal: None,
            wire_task_id: Some("wire-sender-c3".into()),
            ticket: None,
            parent_session: None,
        };
        let _ = &mut ask;
        let tok = crate::providers::CancelToken::new();
        let watch = CancelWatch::token(tok.clone());
        let url2 = url.clone();
        let ask2 = ask.clone();
        let handle = crate::rt::spawn(async move { run_on_peer(&url2, None, &ask2, watch).await });
        // Let the send land and stall, then flip the watch.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        tok.cancel();
        let v = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("run_on_peer must return on cancel, not ride the stalled send")
            .unwrap()
            .expect("the cancel path is Ok, not Err");
        assert_eq!(v["status"], "cancelled", "{v}");
        assert_eq!(v["remote_state"], "TASK_STATE_CANCELED", "{v}");
        assert_eq!(
            saw_cancel.lock().unwrap().as_deref(),
            Some("wire-sender-c3"),
            "the cancel POST carried the sender-minted wire id"
        );
    }
}
