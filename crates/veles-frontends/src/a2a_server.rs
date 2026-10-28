//! A2A inbound server — JSON-RPC over axum (v1.0 binding).
//!
//! Live-session injection (hermes #11025 insight): inbound tasks run in a
//! persistent agent session keyed by the A2A contextId, so multi-turn
//! peer conversations keep memory. Replies return directly on the
//! request — the caller blocks until the turn finishes.
//!
//! Methods served: message/send, tasks/get, tasks/list, tasks/cancel.
//! The Agent Card advertises streaming:false, so message/stream answers
//! with JSON-RPC method-not-found rather than a half-baked SSE stream.
//!
//! Security (all on by default, hermes parity):
//!   - bind safety: no token ⇒ 127.0.0.1; token alone does NOT widen bind
//!   - per-peer tokens (A2A_PEER_TOKENS) → authenticated identity
//!   - trust gate (A2A_TRUSTED_PEERS) → -32052
//!   - sliding-window rate limit (A2A_RATE_LIMIT/min) → -32051
//!   - anti-ping-pong turn cap per context (default 5) → REJECTED
//!   - injection filter + untrusted framing on ALL inbound text
//!   - audit log ~/.local/share/veles/a2a_audit.jsonl

use anyhow::Result;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use veles_core::a2a::{
    authenticate, build_agent_card, build_task, extract_context_id, extract_text, is_trusted_peer,
    jsonrpc_error, jsonrpc_result, new_context_id, new_task_id, redact_outbound, resolve_bind_host,
    send_message_response, wrap_inbound, RateLimiter, TaskRec, TaskStore, TurnTracker,
    ERR_RATE_LIMITED, ERR_TASK_NOT_CANCELABLE, ERR_TASK_NOT_FOUND, ERR_UNTRUSTED, STATE_CANCELED,
    STATE_COMPLETED, STATE_FAILED, STATE_INPUT_REQUIRED, STATE_REJECTED, STATE_SUBMITTED,
    STATE_WORKING,
};
use veles_core::agent::Agent;
use veles_core::config::VelesConfig;
use veles_core::hooks::HookBus;
use veles_core::lockutil::LockOk;
use veles_core::providers::ChatMessage;
use veles_core::store::Store;
use veles_core::tools;

pub fn default_port(cfg: &VelesConfig) -> u16 {
    // Env override first (deployment/testing next to another A2A endpoint).
    if let Some(p) =
        veles_core::providers::resolve_env_or_dotenv("A2A_PORT", &veles_core::config::config_dir())
    {
        if let Ok(n) = p.parse::<u16>() {
            return n;
        }
    }
    cfg.platforms
        .get("a2a")
        .and_then(|a| a.get("extra"))
        .and_then(|e| e.get("port"))
        .and_then(|p| p.as_u64())
        .unwrap_or(9900) as u16
}

pub fn a2a_enabled(cfg: &VelesConfig) -> bool {
    cfg.platforms
        .get("a2a")
        .and_then(|a| a.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// One persistent session per A2A contextId.
struct CtxSession {
    /// tokio Mutex: the guard is held across `run_turn().await` — a
    /// std MutexGuard there would make the handler future non-Send.
    agent: tokio::sync::Mutex<Agent>,
    history: Mutex<Vec<ChatMessage>>,
    /// Serializes turns within one context (per-context FIFO parity).
    turn_lock: tokio::sync::Mutex<()>,
}

pub struct A2AServer {
    cfg: Arc<VelesConfig>,
    store: Arc<Mutex<Store>>,
    hooks: Arc<HookBus>,
    sessions: Mutex<HashMap<String, Arc<CtxSession>>>,
    tasks: TaskStore,
    turns: TurnTracker,
    rate: RateLimiter,
}

impl A2AServer {
    pub fn new(cfg: Arc<VelesConfig>, store: Arc<Mutex<Store>>, hooks: Arc<HookBus>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            store,
            hooks,
            sessions: Mutex::new(HashMap::new()),
            tasks: TaskStore::new(),
            turns: TurnTracker::new(),
            rate: RateLimiter::new(),
        })
    }

    fn session_for(self: &Arc<Self>, context_id: &str) -> Result<Arc<CtxSession>> {
        let key = context_id.to_string();
        if let Some(s) = self.sessions.lock_ok().get(&key) {
            return Ok(s.clone());
        }
        // Registry: base tools + delegate_task (no cronjob — peers must
        // not schedule jobs on us; the depth guard caps delegation).
        let sid = format!("a2a_{context_id}");
        let mut registry = tools::base_registry(self.store.clone(), &sid);
        registry.register(Arc::new(veles_core::delegate::DelegateTaskTool {
            session_id: sid.clone(),
            hooks: None,
        }));
        self.store.lock_ok().create_session(&sid)?;
        let history = self
            .store
            .lock()
            .unwrap()
            .load_history(&sid)
            .unwrap_or_default();
        let agent = Agent::new(
            self.cfg.clone(),
            self.hooks.clone(),
            Arc::new(registry),
            self.store.clone(),
            sid,
        )?;
        let sess = Arc::new(CtxSession {
            agent: tokio::sync::Mutex::new(agent),
            history: Mutex::new(history),
            turn_lock: tokio::sync::Mutex::new(()),
        });
        self.sessions.lock_ok().insert(key, sess.clone());
        Ok(sess)
    }

    /// Run one inbound message/send to completion. Returns the v1.0 Task.
    async fn run_task(self: &Arc<Self>, params: &Value, peer: &str, client_ip: &str) -> Value {
        let mut context_id = extract_context_id(params);
        if context_id.is_empty() {
            context_id = new_context_id();
        }
        let raw_text = extract_text(params);
        let task_id = new_task_id();

        // ── lockdown ──
        //
        // A2A is a connector, and unlike IRC and Telegram it does not
        // pass through the gateway — it holds its own `Agent` and calls
        // `run_turn` itself, so the gateway's gate never sees it. USER,
        // 2026-09-09: *"only local (tui, desktop, repl) frontends are
        // allowed to do anything."* A peer is told once, in the task
        // record, because an RPC caller has nowhere else to read it.
        if let Some(l) = veles_core::lockdown::current(Some(&self.store)) {
            self.tasks.put(TaskRec {
                id: task_id.clone(),
                context_id: context_id.clone(),
                state: STATE_REJECTED.into(),
                peer: peer.to_string(),
                inbound_text: raw_text.clone(),
                reply_text: String::new(),
                created: veles_core::a2a::now_iso(),
                cancel: Default::default(),
            });
            veles_core::a2a::audit_log(&json!({
                "event": "task_rejected", "task_id": task_id,
                "context_id": context_id, "peer": peer,
                "reason": "lockdown engaged",
            }));
            return build_task(
                &task_id,
                &context_id,
                STATE_REJECTED,
                &veles_core::lockdown::refusal(&l),
            );
        }

        // An ask relay (askrelay.rs): a peer's question for the owner,
        // put in THIS node's home chat — answered inline, no session,
        // no turn.
        if veles_core::askrelay::is_ask_relay(&raw_text) {
            return self
                .receive_ask_relay(&task_id, &context_id, peer, &raw_text)
                .await;
        }

        // A memory sync offer is DATA for the operator, not a message
        // for the agent — answered here, before any session or turn
        // exists (see memsync.rs, "Transport").
        if veles_core::memsync::is_offer_message(&raw_text) {
            return self
                .receive_memsync_offer(&task_id, &context_id, peer, &raw_text)
                .await;
        }

        // A store sync offer is data too (B5): a peer's session history
        // for a replica file, its own marker/inbox/gate.
        if veles_core::storesync::is_store_offer(&raw_text) {
            return self.receive_storesync_offer(&task_id, &context_id, peer, &raw_text);
        }

        // A session-migration bundle (B6): gated by its OWN list,
        // `[cluster] migrate_peers`, and an allowlisted push
        // AUTO-ADOPTS (USER, 2026-09-28: the allowlist IS the
        // target's consent — the operator already commanded the
        // migration at the source; no inbox step, no second action).
        // The bundle carries history that will be fed to an LLM here,
        // which is why it is neither a sync peer's nor a task peer's
        // business by default.
        if veles_core::migration::is_migration_message(&raw_text) {
            return self.receive_migration(&task_id, &context_id, peer, &raw_text);
        }

        // A migration query ("did you adopt session X?"): the source's
        // split-brain check before it may un-park or reclaim. Answered
        // from the store, one line, no session/turn — and gated by the
        // same list (only a migration peer may ask).
        if veles_core::migration::is_migration_query(&raw_text) {
            return self.receive_migration_query(&task_id, &context_id, peer, &raw_text);
        }

        // A config sync offer is data too (B2): same treatment, its own
        // marker, its own inbox.
        if veles_core::configsync::is_config_offer(&raw_text) {
            return self.receive_configsync_offer(&task_id, &context_id, peer, &raw_text);
        }

        // A vault offer or join request: data again, never a turn. The
        // payload is age ciphertext and signed rows; storing it decides
        // nothing — `veles vault apply --inbox` verifies every signature.
        if veles_core::vault::is_vault_message(&raw_text) {
            return self.receive_vault_message(&task_id, &context_id, peer, &raw_text);
        }

        // A cluster gossip exchange (B3a): registry rows merged on
        // receipt (the merge is idempotent and conflict-free), answered
        // inline with our rows + the peer-observed-address echo. Gated
        // by the same [cluster] memsync_peers allowlist as the sync
        // inboxes — cluster business, one list.
        if veles_core::gossip::is_gossip_message(&raw_text) {
            return self.receive_gossip(&task_id, &context_id, peer, client_ip, &raw_text);
        }

        // A singleton lease ask (B3a arbiter): answered inline, no
        // session, no turn. Only the ARBITER node serves these; the
        // peer must be allowlisted AND the node id it asks for must be
        // its own (bound via the observation gossip recorded) — an
        // authenticated peer must not be able to hold or release
        // ANOTHER node's lease.
        if veles_core::gossip::is_lease_ask(&raw_text) {
            return self.receive_lease_ask(&task_id, &context_id, peer, client_ip, &raw_text);
        }

        // A remote delegate ask (B4 placement): run a LOCAL subagent
        // child for a pool peer and answer inline with its verdict.
        // Like every cluster protocol: no session, no turn; gated by
        // `[cluster] task_peers`. The child is the same `veles chat
        // -q` runner a local delegate spawns — one code path, the peer
        // just gets to aim it.
        if veles_core::remotetask::is_remote_task(&raw_text) {
            return self
                .receive_remote_task(&task_id, &context_id, peer, &raw_text)
                .await;
        }

        // A re-homed tool call (D1): one tool, run here for a pool peer
        // and answered inline — no session, no turn. Same allowlist as
        // remote subagents; what runs without a ticket is remotetool.rs's
        // rule (the User level, browser tools, host reads outside the
        // secrets).
        if veles_core::remotetool::is_remote_tool(&raw_text) {
            return self
                .receive_remote_tool(&task_id, &context_id, peer, &raw_text)
                .await;
        }

        // Session mail from a pool peer (PLAN-sessions-and-phone.md S4):
        // queued into a session HERE and answered inline — no session or
        // turn for the A2A task itself. Same allowlist as remote work;
        // the mail carries at most what the peer holds here.
        if veles_core::mailnet::is_ask(&raw_text) {
            return self
                .receive_session_mail(&task_id, &context_id, peer, &raw_text)
                .await;
        }

        // The arbiter introducing a node that just joined the pool
        // (veles_core::pool, USER decision D8): only this node's arbiter
        // may, and only over the link this node dials it at.
        if veles_core::pool::is_member(&raw_text) {
            return self
                .receive_pool_member(&task_id, &context_id, peer, &raw_text)
                .await;
        }

        // A node coming back from a lost home asking for its own image
        // (`veles recover`, PLAN-recover.md): data, answered inline — no
        // session, no turn. `recover::serve` binds the peer to a node and
        // gives it that node's data only.
        if veles_core::recover::is_ask(&raw_text) {
            return self
                .receive_recover_ask(&task_id, &context_id, peer, &raw_text)
                .await;
        }
        // A join comes with its invite's own bearer, before any peer
        // token exists (`rpc`). One from a registered peer is an invite
        // used already — its bearer became that peer's token — and never
        // a turn.
        if veles_core::pool::is_join(&raw_text) {
            let reply = json!({
                "status": "refused",
                "error": format!("this invite was used already: {peer} is a peer here now"),
            });
            let enc = veles_core::pool::encode_reply(&reply)
                .unwrap_or_else(|_| "this invite was used already".into());
            return build_task(&task_id, &context_id, STATE_REJECTED, &enc);
        }

        // Anti-ping-pong: cap inbound turns per context.
        if !self.turns.allow(&context_id) {
            self.tasks.put(TaskRec {
                id: task_id.clone(),
                context_id: context_id.clone(),
                state: STATE_REJECTED.into(),
                peer: peer.to_string(),
                inbound_text: raw_text.clone(),
                reply_text: String::new(),
                created: veles_core::a2a::now_iso(),
                cancel: Default::default(),
            });
            veles_core::a2a::audit_log(&json!({
                "event": "task_rejected", "task_id": task_id,
                "context_id": context_id, "peer": peer,
                "reason": "ping-pong turn cap exceeded",
            }));
            return build_task(
                &task_id,
                &context_id,
                STATE_REJECTED,
                "turn limit for this context exceeded — tasks/cancel resets it",
            );
        }

        self.tasks.put(TaskRec {
            id: task_id.clone(),
            context_id: context_id.clone(),
            state: STATE_SUBMITTED.into(),
            peer: peer.to_string(),
            inbound_text: raw_text.clone(),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "task_received", "task_id": task_id,
            "context_id": context_id, "peer": peer,
            "text_chars": raw_text.len(),
        }));
        self.tasks.set_state(&task_id, STATE_WORKING);
        persist_conversation(&context_id, peer, "inbound", &raw_text);

        // Untrusted-content framing, then a normal agent turn in the
        // persistent context session.
        let wrapped = wrap_inbound(peer, &raw_text);
        let sess = match self.session_for(&context_id) {
            Ok(s) => s,
            Err(e) => {
                self.tasks.set_state(&task_id, STATE_FAILED);
                return build_task(
                    &task_id,
                    &context_id,
                    STATE_FAILED,
                    &format!("session error: {e:#}"),
                );
            }
        };
        // Bind the session to the peer BEFORE the turn. Until 2026-09-11
        // an A2A session had no principal at all, and no principal IS
        // the owner surface — so any authenticated peer ran an
        // owner-equivalent agent and neither the media gate nor the F1
        // read jail could bind. Re-bound per task on purpose — a context
        // id is not an identity, and if two peers ever share one, each
        // turn is attributed to the peer that sent it.
        {
            let agent = sess.agent.lock().await;
            agent.set_principal(peer_principal(peer));
        }
        let _fifo = sess.turn_lock.lock().await;
        let mut history = sess.history.lock_ok().clone();
        // The task's own token, so `tasks/cancel` reaches THIS turn.
        // `run_turn` has taken an `Option<CancelToken>` all along and
        // checks it per streamed event; this call passed `None`, which
        // is the entire reason the A2A cancel was cosmetic.
        let cancel = self.tasks.get(&task_id).map(|r| r.cancel);
        let res = {
            let agent = sess.agent.lock().await;
            agent
                .run_turn(&mut history, &wrapped, cancel, |_ev| {})
                .await
        };
        *sess.history.lock_ok() = history;

        // There is deliberately no TASK_STATE_INPUT_REQUIRED branch
        // here (one existed until 2026-09-03 and could never fire):
        // A2A sessions carry no asker, so approvals fall back to
        // `approvals.non_interactive` and clarify answers "no
        // interactive user is available" inside the reply. A turn
        // either completes or fails — mid-turn questioning of a peer
        // would need turn suspension this server does not have. The
        // state stays in a2a.rs for the CLIENT side, where a peer we
        // call may legitimately return it.
        let (state, reply) = match res {
            Ok(tr) => (STATE_COMPLETED, tr.reply),
            Err(e) => (STATE_FAILED, format!("agent error: {e:#}")),
        };
        self.tasks.set_state(&task_id, state);
        self.tasks.set_reply(&task_id, &reply);
        persist_conversation(&context_id, "veles", "outbound", &reply);
        veles_core::a2a::audit_log(&json!({
            "event": "task_completed", "task_id": task_id,
            "context_id": context_id, "state": state,
            "reply_chars": reply.len(),
        }));
        build_task(&task_id, &context_id, state, &redact_outbound(&reply))
    }

    /// Store a memory sync offer in the inbox — or refuse it — without
    /// a session and without a turn. Storing is not applying: the reply
    /// names the file and the command that applies it.
    ///
    /// Only peers named in `[cluster] memsync_peers` may deposit: memory
    /// that arrives from a peer the operator never named is not kept on
    /// disk at all. Read LIVE (not the boot snapshot) so adding a peer
    /// needs no restart. The task record keeps a size line, not the
    /// payload, so a 700 KB chunk is not also held in the task table.
    async fn receive_memsync_offer(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let allowed = veles_core::config::load()
            .map(|c| c.cluster.memsync_peers)
            .unwrap_or_default();
        let (state, reply) = if !allowed.iter().any(|p| p == peer) {
            (
                STATE_REJECTED,
                format!(
                    "not stored: this node keeps memory sync offers only from peers named in \
                     [cluster] memsync_peers, and '{peer}' is not one"
                ),
            )
        } else {
            match veles_core::memsync::decode_message(raw) {
                Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                Ok(offer) => {
                    let summary = veles_core::memsync::summarize_offer(&offer);
                    let dir = veles_core::memsync::inbox_dir();
                    match veles_core::memsync::inbox_store(&dir, peer, task_id, offer) {
                        Ok(path) => {
                            let file = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            // Read like memsync_peers above: from the
                            // file, so turning it on needs no restart.
                            let auto = veles_core::config::load()
                                .map(|c| c.cluster.memsync_auto)
                                .unwrap_or(false);
                            let reply = if auto {
                                apply_memsync_inbox().await
                            } else {
                                "nothing applied; the operator applies it with `veles \
                                 memory apply --inbox`"
                                    .to_string()
                            };
                            (
                                STATE_COMPLETED,
                                format!("stored {summary} as {file} — {reply}"),
                            )
                        }
                        Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                    }
                }
            }
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[memory sync offer, {} bytes]", raw.len()),
            reply_text: reply.clone(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "memsync_offer", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
            "bytes": raw.len(),
        }));
        build_task(task_id, context_id, state, &reply)
    }

    /// Put a peer's question to the owner here (`askrelay::serve`), for a
    /// peer named in `[cluster] task_peers` — the list of peers whose
    /// work this node does — and answer with what the owner chose. The
    /// call stays open while the question does; the asking node's own
    /// timeout covers it.
    async fn receive_ask_relay(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let allowed = veles_core::config::load()
            .map(|c| c.cluster.task_peers)
            .unwrap_or_default();
        let (state, reply) = if !allowed.iter().any(|p| p == peer) {
            (
                STATE_REJECTED,
                format!(
                    "not relayed: this node puts questions to its owner only for peers named \
                     in [cluster] task_peers, and '{peer}' is not one"
                ),
            )
        } else {
            match veles_core::askrelay::decode(raw) {
                Err(e) => (STATE_REJECTED, format!("not relayed: {e:#}")),
                Ok(wire) => match veles_core::askrelay::serve(wire).await {
                    Ok(answer) => (
                        STATE_COMPLETED,
                        veles_core::askrelay::encode_answer(&answer),
                    ),
                    Err(why) => (STATE_REJECTED, format!("not relayed: {why}")),
                },
            }
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[ask relay, {} bytes]", raw.len()),
            reply_text: reply.clone(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "ask_relay", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
        }));
        build_task(task_id, context_id, state, &reply)
    }

    /// Store a store sync offer in the inbox — or refuse it — same
    /// shape as the memory/config sides (B5). The gate is its own list,
    /// `[cluster] storesync_peers`: a store offer carries a peer's
    /// whole session history into a replica file, a trust decision of
    /// its own. Read LIVE so adding a peer needs no restart.
    fn receive_storesync_offer(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let allowed = veles_core::config::load()
            .map(|c| c.cluster.storesync_peers)
            .unwrap_or_default();
        let mut from_node = String::new();
        let (state, reply) = if !allowed.iter().any(|p| p == peer) {
            (
                STATE_REJECTED,
                format!(
                    "not stored: this node keeps store sync offers only from peers named in \
                     [cluster] storesync_peers, and '{peer}' is not one"
                ),
            )
        } else {
            // The offer's `from_node` picks the replica file it writes,
            // and it is the sender's own claim — bound to the
            // authenticated peer before anything touches disk.
            let bound = |offer: &veles_core::storesync::StoreOffer| {
                let store = self.store.lock_ok();
                let me = self_node_id(&store);
                veles_core::storesync::check_sender(&store, peer, &offer.from_node, &me)
            };
            match veles_core::storesync::decode_message(raw)
                .and_then(|offer| bound(&offer).map(|()| offer))
            {
                Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                Ok(offer) => {
                    let n = offer.rows.len();
                    from_node = offer.from_node.clone();
                    let from = offer.from_node.clone();
                    let dir = veles_core::storesync::inbox_dir();
                    match veles_core::storesync::inbox_store(&dir, peer, task_id, offer) {
                        Ok(path) => {
                            let cur = veles_core::storesync::replica_cursor(&from);
                            (
                                STATE_COMPLETED,
                                format!(
                                    "stored {n} row(s) from {from} as {} — nothing applied; the \
                                     operator applies it with `veles replicas apply --inbox` \
                                     (replica cursor {cur})",
                                    path.file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_default()
                                ),
                            )
                        }
                        Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                    }
                }
            }
        };
        // AUTO (this receiver's OWN flag, read live): apply what was
        // just stored in the same round trip — oldest-first, which is
        // arrival order, so chunks of one push apply in send order. A
        // refusal is not a rejection of the STORE (it succeeded); the
        // offer stays pending in the inbox. A gap refusal on THIS
        // sender's replica becomes a heal directive the sender can act
        // on (`gap: re-push past N`); anything else is named for the
        // operator.
        let mut final_reply = reply.clone();
        if veles_core::config::load()
            .map(|c| c.cluster.storesync_auto)
            .unwrap_or(false)
            && state == STATE_COMPLETED
        {
            let (applied, refusals) = veles_core::storesync::apply_inbox();
            let cur = veles_core::storesync::replica_cursor(&from_node);
            final_reply = if refusals.is_empty() {
                format!("{final_reply}\napplied automatically — replica cursor {cur}")
            } else {
                let mut parts = Vec::new();
                for r in &refusals {
                    if r.sender_node == from_node && veles_core::storesync::is_gap_refusal(&r.msg) {
                        parts.push(format!(
                            "{}{cur} (re-push the missing range, oldest first)",
                            veles_core::storesync::HEAL_MARKER
                        ));
                    } else {
                        parts.push(format!("{}: {}", r.sender_node, r.msg));
                    }
                }
                format!(
                    "{final_reply}\nauto-apply: {applied} offer(s) applied, {} refused and \
                     still pending in the inbox — replica cursor {cur}: {}",
                    refusals.len(),
                    parts.join(" | ")
                )
            };
            veles_core::a2a::audit_log(&json!({
                "event": "storesync_auto_apply", "task_id": task_id,
                "peer": peer, "applied": applied, "refused": refusals.len(),
                "replica_cursor": cur,
            }));
        }
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[store sync offer, {} bytes]", raw.len()),
            reply_text: final_reply.clone(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "storesync_offer", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
            "bytes": raw.len(),
        }));
        build_task(task_id, context_id, state, &final_reply)
    }

    /// Store a config sync offer in the inbox — or refuse it — same
    /// shape as the memory side (B2). The trust gate is the SAME
    /// list, `[cluster] memsync_peers`: one list of "peers allowed to
    /// deposit sync data in my inboxes", so a peer added for memory is
    /// a peer trusted with config too, and a peer not named deposits
    /// nothing anywhere.
    fn receive_configsync_offer(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let allowed = veles_core::config::load()
            .map(|c| c.cluster.memsync_peers)
            .unwrap_or_default();
        let (state, reply) = if !allowed.iter().any(|p| p == peer) {
            (
                STATE_REJECTED,
                format!(
                    "not stored: this node keeps config sync offers only from peers named in \
                     [cluster] memsync_peers, and '{peer}' is not one"
                ),
            )
        } else {
            match veles_core::configsync::decode_message(raw) {
                Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                Ok(offer) => {
                    let summary = veles_core::configsync::summarize_offer(&offer);
                    let dir = veles_core::configsync::inbox_dir();
                    match veles_core::configsync::inbox_store(&dir, peer, task_id, offer) {
                        Ok(path) => {
                            let file = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            // Read like memsync_peers above: from the
                            // file, so turning it off needs no restart.
                            let auto = veles_core::config::load()
                                .map(|c| c.cluster.configsync_auto)
                                .unwrap_or(false);
                            let tail = if auto {
                                self.apply_configsync_inbox()
                            } else {
                                "nothing applied; the operator applies it with `veles \
                                 configsync apply --inbox`"
                                    .to_string()
                            };
                            (
                                STATE_COMPLETED,
                                format!("stored {summary} as {file} — {tail}"),
                            )
                        }
                        Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                    }
                }
            }
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[config sync offer, {} bytes]", raw.len()),
            reply_text: reply.clone(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "configsync_offer", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
            "bytes": raw.len(),
        }));
        build_task(task_id, context_id, state, &reply)
    }
}

/// `[cluster] configsync_auto` carries the vault: merge the vault inbox
/// into this node's vault now (`vault::apply_inbox_auto`), log each
/// outcome — names only, never a value — and say in one line what
/// happened, for the sender's log. Also the sync loop's drain.
pub(crate) fn apply_vault_inbox() -> String {
    use veles_core::vault::AutoOutcome;
    match veles_core::vault::apply_inbox_auto() {
        Err(e) => {
            tracing::warn!("vault: applying the inbox failed: {e:#}");
            format!("stored, not applied: {e:#}")
        }
        Ok(done) => {
            let (mut merged, mut waiting, mut refused) = (0, 0, 0);
            for d in &done {
                match &d.outcome {
                    AutoOutcome::Merged(r) => {
                        merged += 1;
                        if !r.changed.is_empty() || !r.refused.is_empty() {
                            tracing::info!(
                                "vault: merged an offer from {}: {}",
                                d.peer,
                                r.summarize()
                            );
                        }
                    }
                    AutoOutcome::NoVault => waiting += 1,
                    AutoOutcome::Refused(e) => {
                        refused += 1;
                        tracing::warn!(
                            "vault: an offer from {} stays in the inbox ({}): {e}",
                            d.peer,
                            d.file
                        );
                    }
                }
            }
            if waiting > 0 {
                tracing::info!(
                    "vault: {waiting} offer(s) wait in the inbox — this node has no vault yet; \
                     adopt it with `veles vault apply --inbox --trust <root fingerprint>`"
                );
                return "kept: this node has no vault yet; its operator adopts it with \
                        `veles vault apply --inbox --trust <root fingerprint>`"
                    .to_string();
            }
            if refused > 0 {
                return format!("merged {merged}, refused {refused} (kept in the inbox)");
            }
            format!("merged ({merged} offer(s))")
        }
    }
}

impl A2AServer {
    fn apply_configsync_inbox(&self) -> String {
        apply_configsync_inbox(&veles_core::store::lock_timed(
            &self.store,
            "a2a::configsync_apply",
        ))
    }
}

/// `[cluster] configsync_auto`, the receiving half: apply the inbox
/// now. Rows for a security section wait for the owner, and an offer
/// with a conflict stays whole — both are logged, counted in
/// `/api/status` and listed by `/configsync`. Also the sync loop's
/// drain, for an offer that arrived while this node could not apply it.
pub(crate) fn apply_configsync_inbox(store: &veles_core::store::Store) -> String {
    let me = match veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())
    {
        Ok(i) => i.id,
        Err(e) => return format!("stored, not applied: {e:#}"),
    };
    match veles_core::configsync::apply_inbox_auto(store, &me) {
        Ok(done) => {
            let mut held = 0;
            let mut conflicts = 0;
            for d in &done {
                if !d.report.changed.is_empty() {
                    tracing::info!(
                        "configsync: applied from {}: {}",
                        d.from,
                        d.report.changed.join(", ")
                    );
                }
                if !d.report.is_clean() {
                    conflicts += 1;
                    tracing::warn!(
                        "configsync: an offer from {} conflicts and stays in the inbox \
                             (the winner is written) — `/configsync` lists it: {}",
                        d.from,
                        d.report.summarize()
                    );
                }
                if !d.held.is_empty() {
                    held += d.held.len();
                    tracing::warn!(
                        "configsync: {} change(s) from {} touch a security section and \
                             wait for the owner — asked on the home chat when [notify] \
                             ask_relay is on, else `/configsync apply`: {}",
                        d.held.len(),
                        d.from,
                        d.held.join(", ")
                    );
                }
            }
            if held == 0 && conflicts == 0 {
                format!("applied ({} offer(s))", done.len())
            } else {
                format!(
                    "applied; {held} change(s) to a security section and {conflicts} \
                         conflicting offer(s) wait for the owner"
                )
            }
        }
        Err(e) => {
            tracing::warn!("configsync: applying the inbox failed: {e:#}");
            format!("stored, not applied: {e:#}")
        }
    }
}

impl A2AServer {
    /// Store a vault message in the vault inbox — or refuse it. Same
    /// trust list as the other sync inboxes (`[cluster] memsync_peers`);
    /// the task record keeps a size line, not the payload.
    fn receive_vault_message(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let allowed = veles_core::config::load()
            .map(|c| c.cluster.memsync_peers)
            .unwrap_or_default();
        let (state, reply) = if !allowed.iter().any(|p| p == peer) {
            (
                STATE_REJECTED,
                format!(
                    "not stored: this node keeps vault messages only from peers named in \
                     [cluster] memsync_peers, and '{peer}' is not one"
                ),
            )
        } else {
            match veles_core::vault::decode_message(raw) {
                Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                Ok(msg) => {
                    // Read like memsync_peers above: from the file, so
                    // turning it off needs no restart.
                    let auto = veles_core::config::load()
                        .map(|c| c.cluster.configsync_auto)
                        .unwrap_or(false);
                    let offered = match &msg {
                        veles_core::vault::Message::Offer { file, .. } => {
                            Some(veles_core::vault::rows_digest(file))
                        }
                        veles_core::vault::Message::Join(_) => None,
                    };
                    let what = match &msg {
                        veles_core::vault::Message::Offer { .. } => "a vault offer".to_string(),
                        veles_core::vault::Message::Join(r) => format!(
                            "a join request from {} (fingerprint {}) — nothing admitted; the \
                             operator admits it with `veles vault admit`",
                            r.name,
                            r.fingerprint()
                        ),
                    };
                    match veles_core::vault::inbox_store(
                        &veles_core::vault::inbox_dir(),
                        peer,
                        task_id,
                        msg,
                    ) {
                        Ok(_) => {
                            let tail = match offered {
                                None => String::new(),
                                Some(_) if !auto => " — nothing applied; the operator applies \
                                     it with `veles vault apply --inbox`"
                                    .to_string(),
                                Some(digest) => {
                                    // The sender holds these rows: the loop
                                    // does not send them back.
                                    let key = veles_core::vault::peer_has_key(peer);
                                    if let Err(e) = self.store.lock_ok().meta_set(&key, &digest) {
                                        tracing::warn!("vault: noting what {peer} holds: {e:#}");
                                    }
                                    format!(" — {}", apply_vault_inbox())
                                }
                            };
                            (STATE_COMPLETED, format!("stored {what}{tail}"))
                        }
                        Err(e) => (STATE_REJECTED, format!("not stored: {e:#}")),
                    }
                }
            }
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[vault message, {} bytes]", raw.len()),
            reply_text: reply.clone(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "vault_message", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
            "bytes": raw.len(),
        }));
        build_task(task_id, context_id, state, &reply)
    }

    /// A cluster gossip exchange (B3a): merge the peer's registry rows
    /// on receipt and answer inline with ours + the address echo. The
    /// allowlist gate and the observation recording live in
    /// `gossip::receive_gossip`; this wrapper keeps the task record and
    /// audit line in the same shape as every other data message.
    fn receive_gossip(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        client_ip: &str,
        raw: &str,
    ) -> Value {
        // Read LIVE (not the boot snapshot) so adding a peer needs no
        // restart — same reasoning as the sync inboxes. A config that
        // fails to load refuses the exchange (an unreadable allowlist
        // must fail closed, not default to empty).
        let (state, reply) = {
            // The config is read before the store lock is taken: it is a
            // file read and a parse of ~50 KB of TOML (8 ms in a debug
            // build), and this ran every 30 s per peer with the lock held.
            let r = veles_core::config::load()
                .map_err(|e| anyhow::anyhow!("config load failed: {e:#}"))
                .and_then(|c| {
                    let store = veles_core::store::lock_timed(&self.store, "a2a::gossip");
                    veles_core::gossip::receive_gossip(&store, &c, peer, client_ip, raw)
                });
            match r {
                Ok(reply_text) => (STATE_COMPLETED, reply_text),
                Err(e) => (STATE_REJECTED, format!("gossip refused: {e:#}")),
            }
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[cluster gossip, {} bytes]", raw.len()),
            reply_text: reply.clone(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "cluster_gossip", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
            "observed_addr": client_ip, "bytes": raw.len(),
        }));
        build_task(task_id, context_id, state, &reply)
    }

    /// A singleton lease ask (B3a): this node is the arbiter, the peer
    /// is asking to hold (or renew, or read) a singleton lease. The
    /// ask's `holder` must be the node id this PEER owns — bound by the
    /// observation gossip recorded (peer name → node id) — so an
    /// authenticated peer cannot grab or release another node's lease.
    fn receive_lease_ask(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        client_ip: &str,
        raw: &str,
    ) -> Value {
        let ask = match veles_core::gossip::decode_lease_ask(raw) {
            Ok(a) => a,
            Err(e) => {
                let reply = format!("lease ask refused: {e:#}");
                self.tasks.put(TaskRec {
                    id: task_id.to_string(),
                    context_id: context_id.to_string(),
                    state: STATE_REJECTED.into(),
                    peer: peer.to_string(),
                    inbound_text: format!("[lease ask, {} bytes]", raw.len()),
                    reply_text: reply.clone(),
                    created: veles_core::a2a::now_iso(),
                    cancel: Default::default(),
                });
                return build_task(task_id, context_id, STATE_REJECTED, &reply);
            }
        };
        // Only the arbiter serves lease asks. Any other node answering
        // would be a second arbiter — the exact failure the design
        // exists to prevent.
        let me_arbiter = {
            let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            veles_core::node::validate_arbiter(&self.cfg.cluster, &store)
                .map(|full| full == self_node_id(&store))
                .unwrap_or(false)
        };
        let (state, reply) = if !me_arbiter {
            (
                STATE_REJECTED,
                "lease ask refused: this node is not the arbiter".to_string(),
            )
        } else {
            // Peer → node id binding: the observation table's latest
            // record for which node id arrives as this peer name. A
            // peer with no binding may not ask at all — the gossip
            // that creates the binding must land first.
            let (verdict, reply) = {
                let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                let allowed = veles_core::config::load()
                    .map(|c| c.cluster.memsync_peers)
                    .unwrap_or_default();
                if !allowed.iter().any(|p| p == peer) {
                    (
                        STATE_REJECTED,
                        format!("lease ask refused: '{peer}' is not in [cluster] memsync_peers"),
                    )
                } else {
                    match peer_node_binding(&store, peer) {
                        Some(bound) if bound == ask.holder => {
                            // Bound and honest: serve the ask.
                            let answer = if ask.acquire {
                                veles_core::arbiter::grant_local(
                                    &store,
                                    &ask.key,
                                    &ask.holder,
                                    chrono::Utc::now(),
                                )
                            } else {
                                store
                                    .lease_get(&ask.key, chrono::Utc::now())
                                    .ok()
                                    .flatten()
                                    .map(|(h, _, u)| {
                                        if h == ask.holder {
                                            veles_core::arbiter::LeaseState::Held(u)
                                        } else {
                                            veles_core::arbiter::LeaseState::HeldByOther {
                                                holder: h,
                                                until: u,
                                            }
                                        }
                                    })
                                    .unwrap_or_else(|| {
                                        veles_core::arbiter::LeaseState::Refused("no lease".into())
                                    })
                            };
                            let wire = veles_core::arbiter::LeaseAnswerWire::from(answer);
                            (
                                STATE_COMPLETED,
                                veles_core::gossip::encode_lease_answer(&wire),
                            )
                        }
                        Some(bound) => (
                            STATE_REJECTED,
                            format!(
                                "lease ask refused: peer '{peer}' is bound to node {bound}, \
                                 not {holder} — a peer may only ask for its own node's lease",
                                holder = ask.holder
                            ),
                        ),
                        None => (
                            STATE_REJECTED,
                            format!(
                                "lease ask refused: no gossip binding for peer '{peer}' yet — \
                                 exchange registry rows first (the binding lands with the \
                                 first gossip exchange)"
                            ),
                        ),
                    }
                }
            };
            let _ = client_ip;
            (verdict, reply)
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[lease ask {}:{}]", ask.key, ask.holder),
            reply_text: reply.clone(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "lease_ask", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
            "key": ask.key, "holder": ask.holder,
        }));
        build_task(task_id, context_id, state, &reply)
    }

    /// A remote delegate ask (B4): a pool peer placed a child HERE.
    ///
    /// Gates, in order: the allowlist (`[cluster] task_peers`, its own
    /// list — see remotetask.rs) and the checksum (decode verifies). Recursion needs no wire check:
    /// the ask can only originate from a depth-guarded delegate call,
    /// and the child spawned below runs at depth 1 with no
    /// delegate_task of its own — one hop by construction. Then the
    /// ask runs through the same local child runner a delegate_task
    /// call uses (`run_child_routed`), and the verdict JSON travels
    /// back fenced.
    ///
    /// The child SESSION here is local to this node (its store, its
    /// session list); its id rides back as `remote_child_session` so
    /// the sender's verdict names something real. B5 (store
    /// replication) is what will make that transcript readable FROM
    /// the sending node; until then the verdict summary is the whole
    /// report that crosses the wire.
    async fn receive_remote_task(
        self: &Arc<Self>,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        // C3: when the sender minted a wire task id, the record it can
        // aim tasks/cancel at is registered WORKING BEFORE the child
        // starts, with a live token threaded into the child's kill
        // loop. Before this, the record only existed AFTER the run —
        // tasks/cancel answered "not found" for the child's whole life
        // and the child burned to its cap. An ask without a wire id
        // (older sender) keeps the B4 shape exactly: one record, put
        // at the end, under our own task id.
        let decoded = veles_core::remotetask::decode_ask(raw);
        let rec_id = decoded
            .as_ref()
            .ok()
            .and_then(|a| a.wire_task_id.clone())
            .unwrap_or_else(|| task_id.to_string());
        let cancel = veles_core::providers::CancelToken::new();
        let (state, reply) = match decoded {
            Err(e) => (
                STATE_REJECTED,
                veles_core::remotetask::encode_reply(&json!({
                    "status": "refused", "error": format!("bad ask: {e:#}")
                }))
                .unwrap_or_else(|_| "remote task refused: bad ask".into()),
            ),
            Ok(ask) => {
                // Its OWN allowlist (USER, 2026-09-28): memsync_peers
                // means "may drop sync offers into my inbox, applied by
                // hand"; running a subagent here — shell, files, keys —
                // is a different grant, off until a peer is named.
                let allowed = veles_core::config::load()
                    .map(|c| c.cluster.task_peers)
                    .unwrap_or_default();
                if !allowed.iter().any(|p| p == peer) {
                    (
                        STATE_REJECTED,
                        format!("remote task refused: '{peer}' is not in [cluster] task_peers"),
                    )
                } else {
                    if ask.wire_task_id.is_some() {
                        self.tasks.put(TaskRec {
                            id: rec_id.clone(),
                            context_id: context_id.to_string(),
                            state: STATE_WORKING.into(),
                            peer: peer.to_string(),
                            inbound_text: format!("[remote task, {} bytes]", raw.len()),
                            reply_text: String::new(),
                            created: veles_core::a2a::now_iso(),
                            cancel: cancel.clone(),
                        });
                    }
                    // Recursion: no wire check needed. The ask can only
                    // originate from a delegate_task call (depth-guarded
                    // on the sender), and the child spawned below runs
                    // with VELES_CHILD_DEPTH=1 — so it has no
                    // delegate_task of its own. One hop by construction;
                    // see remotetask.rs's RemoteTaskAsk doc.
                    let me = self_node_id(&self.store.lock().unwrap_or_else(|p| p.into_inner()));
                    let verdict = {
                        // Same runner and prompt contract as a local
                        // delegate, under the CALLER's principal.
                        let goal = ask.goal.clone();
                        let ctx = ask.context.clone();
                        let (m, p, r) = (
                            ask.model.clone(),
                            ask.provider.clone(),
                            ask.reasoning.clone(),
                        );
                        // D2: the child runs at most at the User level
                        // unless a ticket for exactly this delegation
                        // raises it — before, a peer's owner-surface
                        // delegation ran here as this node's owner.
                        let principal = match self.delegate_principal(peer, &ask) {
                            Ok(p) => p,
                            Err(e) => {
                                let reply = veles_core::remotetask::encode_reply(&json!({
                                    "status": "refused", "error": format!("{e:#}"),
                                }))
                                .unwrap_or_else(|_| "remote task refused".into());
                                self.tasks.put(TaskRec {
                                    id: rec_id.clone(),
                                    context_id: context_id.to_string(),
                                    state: STATE_REJECTED.into(),
                                    peer: peer.to_string(),
                                    inbound_text: format!("[remote task, {} bytes]", raw.len()),
                                    reply_text: reply.chars().take(2000).collect(),
                                    created: veles_core::a2a::now_iso(),
                                    cancel: cancel.clone(),
                                });
                                return build_task(task_id, context_id, STATE_REJECTED, &reply);
                            }
                        };
                        let token = if ask.wire_task_id.is_some() {
                            Some(cancel.clone())
                        } else {
                            None
                        };
                        veles_core::rt::spawn_blocking(move || {
                            veles_core::delegate::run_remote_child(
                                &goal,
                                &ctx,
                                m.as_deref(),
                                p.as_deref(),
                                r.as_deref(),
                                principal.as_deref(),
                                token,
                            )
                        })
                        .await
                        .unwrap_or_else(|e| {
                            json!({
                                "status": "error", "error": format!("join failed: {e}")
                            })
                        })
                    };
                    let mut v = verdict;
                    if !me.is_empty() {
                        v["remote_node"] = json!(me);
                    }
                    // A cancel that landed while the child ran keeps
                    // the record CANCELED — tasks/cancel already told
                    // the peer so, and tasks/get must not contradict
                    // it a moment later. (The verdict TEXT still rides
                    // the reply: the sender may read it anyway.)
                    let st = if cancel.is_cancelled() {
                        STATE_CANCELED
                    } else if v["status"] == "ok" || v["status"] == "cancelled" {
                        STATE_COMPLETED
                    } else {
                        STATE_FAILED
                    };
                    let enc = veles_core::remotetask::encode_reply(&v)
                        .unwrap_or_else(|_| "remote task reply encode failed".into());
                    (st, enc)
                }
            }
        };
        // Update-in-place under the SAME id (TaskStore::put keeps one
        // order entry per id) — the WORKING row the sender could watch
        // becomes the terminal row.
        self.tasks.put(TaskRec {
            id: rec_id,
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[remote task, {} bytes]", raw.len()),
            reply_text: reply.chars().take(2000).collect(),
            created: veles_core::a2a::now_iso(),
            cancel,
        });
        veles_core::a2a::audit_log(&json!({
            "event": "remote_task", "task_id": task_id,
            "context_id": context_id, "peer": peer, "state": state,
            "bytes": raw.len(),
        }));
        build_task(task_id, context_id, state, &reply)
    }
}

impl A2AServer {
    /// Who the authenticated `peer` is, as this node knows it: the node
    /// id gossip bound it to (None until gossip has run) and its name.
    fn caller_of(&self, peer: &str) -> (String, Option<String>, String) {
        let store = self.store.lock_ok();
        let me = self_node_id(&store);
        let bound = peer_node_binding(&store, peer);
        let name = bound
            .as_deref()
            .and_then(|id| store.node_get(id).ok().flatten())
            .map(|n| n.name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| peer.to_string());
        (me, bound, name)
    }

    /// The principal a remote subagent runs under here, serialized for
    /// its child's environment (D2): capped at User unless ticketed.
    fn delegate_principal(
        &self,
        peer: &str,
        ask: &veles_core::remotetask::RemoteTaskAsk,
    ) -> anyhow::Result<Option<String>> {
        let (me, bound, name) = self.caller_of(peer);
        let cfg = veles_core::config::load().unwrap_or_default();
        let caller = veles_core::remotetool::Caller {
            node: bound.as_deref(),
            name: &name,
        };
        let p = veles_core::remotetool::delegate_principal(
            &self.store.lock_ok(),
            &cfg,
            &me,
            &caller,
            ask.principal.as_deref(),
            ask.ticket.as_deref(),
            &ask.goal,
            &ask.context,
            chrono::Utc::now().timestamp(),
        )?;
        Ok(match p {
            Some(p) => Some(serde_json::to_string(&p)?),
            None => None,
        })
    }

    /// One re-homed tool call (D1/D2): gate on `[cluster] task_peers`,
    /// decide what runs unasked or by ticket (remotetool.rs), run it,
    /// answer inline. A sender-minted wire id registers the task
    /// WORKING first, so the caller's Stop can cancel it.
    async fn receive_remote_tool(
        self: &Arc<Self>,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        use veles_core::remotetool::{self as rt, Decision};
        let decoded = rt::decode_ask(raw);
        let rec_id = decoded
            .as_ref()
            .ok()
            .and_then(|a| a.wire_task_id.clone())
            .unwrap_or_else(|| task_id.to_string());
        let cancel = veles_core::providers::CancelToken::new();
        let mut tool = String::new();
        let mut ran_as = String::new();
        let reply = match decoded {
            Err(e) => json!({"status": "refused", "error": format!("bad call: {e:#}")}),
            Ok(ask) => {
                tool = ask.tool.clone();
                let cfg = veles_core::config::load().unwrap_or_default();
                if !cfg.cluster.task_peers.iter().any(|p| p == peer) {
                    json!({
                        "status": "refused",
                        "error": format!("'{peer}' is not in [cluster] task_peers here"),
                    })
                } else {
                    let (me, bound, name) = self.caller_of(peer);
                    let caller = rt::Caller {
                        node: bound.as_deref(),
                        name: &name,
                    };
                    let decision = rt::decide(
                        &self.store.lock_ok(),
                        &cfg,
                        &me,
                        &caller,
                        &ask,
                        chrono::Utc::now().timestamp(),
                    );
                    match decision {
                        Decision::Run(grant) => {
                            ran_as = grant.label();
                            if ask.wire_task_id.is_some() {
                                self.tasks.put(TaskRec {
                                    id: rec_id.clone(),
                                    context_id: context_id.to_string(),
                                    state: STATE_WORKING.into(),
                                    peer: peer.to_string(),
                                    inbound_text: format!("[remote tool {}]", ask.tool),
                                    reply_text: String::new(),
                                    created: veles_core::a2a::now_iso(),
                                    cancel: cancel.clone(),
                                });
                            }
                            rt::execute(self.store.clone(), &caller, &ask, &grant, cancel.clone())
                                .await
                        }
                        Decision::NeedsTicket { why, request } => json!({
                            "status": "needs_ticket", "error": why, "request": request,
                        }),
                        Decision::Refused(why) => json!({"status": "refused", "error": why}),
                    }
                }
            }
        };
        let state = if cancel.is_cancelled() {
            STATE_CANCELED
        } else {
            match reply["status"].as_str() {
                Some("ok") => STATE_COMPLETED,
                Some("error") => STATE_FAILED,
                _ => STATE_REJECTED,
            }
        };
        let enc =
            rt::encode_reply(&reply).unwrap_or_else(|_| "remote tool reply encode failed".into());
        self.tasks.put(TaskRec {
            id: rec_id,
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[remote tool {tool}]"),
            reply_text: enc.chars().take(2000).collect(),
            created: veles_core::a2a::now_iso(),
            cancel,
        });
        veles_core::a2a::audit_log(&json!({
            "event": "remote_tool", "task_id": task_id, "context_id": context_id,
            "peer": peer, "tool": tool, "state": state, "ran_as": ran_as,
        }));
        build_task(task_id, context_id, state, &enc)
    }

    /// The arbiter introducing a newly joined node (`veles_core::pool`).
    async fn receive_pool_member(
        self: &Arc<Self>,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let cfg = veles_core::config::load().unwrap_or_default();
        let reply = match veles_core::pool::decode_member(raw) {
            Err(e) => json!({"status": "refused", "error": format!("bad introduction: {e:#}")}),
            Ok(ask) => {
                let store = self.store.lock_ok();
                veles_core::pool::accept_member(&store, &cfg, peer, ask)
            }
        };
        let state = if reply["status"] == "ok" {
            STATE_COMPLETED
        } else {
            STATE_REJECTED
        };
        let enc = veles_core::pool::encode_reply(&reply)
            .unwrap_or_else(|_| "pool reply encode failed".into());
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: "[pool member]".into(),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "pool_member", "task_id": task_id, "peer": peer,
            "state": state, "error": reply["error"],
        }));
        build_task(task_id, context_id, state, &enc)
    }

    /// One recovery ask (`veles_core::recover::serve`). Off the async
    /// workers: a snapshot is a `VACUUM INTO` of a replica that may be
    /// hundreds of MB. The task record keeps the op, never the payload.
    async fn receive_recover_ask(
        self: &Arc<Self>,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let cfg = veles_core::config::load().unwrap_or_default();
        let store = self.store.clone();
        let (who, text) = (peer.to_string(), raw.to_string());
        let (reply, op) =
            veles_core::rt::spawn_blocking(move || match veles_core::recover::decode_ask(&text) {
                Err(e) => (
                    json!({"status": "refused", "error": format!("bad recovery ask: {e:#}")}),
                    String::new(),
                ),
                Ok(ask) => (veles_core::recover::serve(&store, &cfg, &who, &ask), ask.op),
            })
            .await
            .unwrap_or_else(|e| {
                (
                    json!({"status": "refused", "error": format!("the recovery ask failed: {e}")}),
                    String::new(),
                )
            });
        let state = if reply["status"] == "ok" {
            STATE_COMPLETED
        } else {
            STATE_REJECTED
        };
        let enc = veles_core::recover::encode_reply(&reply)
            .unwrap_or_else(|_| "recovery reply encode failed".into());
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[recover ask: {op}]"),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "recover_ask", "task_id": task_id, "peer": peer, "op": op,
            "state": state, "error": reply["error"],
        }));
        build_task(task_id, context_id, state, &enc)
    }

    /// A node redeeming its pool invite (`veles_core::pool::redeem`). Its
    /// bearer is the invite's own, registered nowhere until this call
    /// succeeds, so `rpc` lets this one call through and nothing else.
    async fn pool_join_rpc(
        self: &Arc<Self>,
        invited: &str,
        auth: &str,
        body: &Value,
    ) -> (StatusCode, Json<Value>) {
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let params = body.get("params").cloned().unwrap_or(json!({}));
        let raw = extract_text(&params);
        if body["method"].as_str() != Some("message/send") || !veles_core::pool::is_join(&raw) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(jsonrpc_error(
                    &id,
                    veles_core::a2a::ERR_UNAUTHORIZED,
                    "unauthorized",
                )),
            );
        }
        let mut context_id = extract_context_id(&params);
        if context_id.is_empty() {
            context_id = new_context_id();
        }
        let task_id = new_task_id();
        let reply = if let Some(l) = veles_core::lockdown::current(Some(&self.store)) {
            json!({"status": "refused", "error": veles_core::lockdown::refusal(&l)})
        } else {
            let cfg = veles_core::config::load().unwrap_or_default();
            let bearer = auth.strip_prefix("Bearer ").unwrap_or("").trim();
            match veles_core::pool::decode_join(&raw) {
                Err(e) => json!({"status": "refused", "error": format!("bad join: {e:#}")}),
                Ok(ask) => veles_core::pool::redeem(&self.store, &cfg, bearer, ask).await,
            }
        };
        let state = if reply["status"] == "ok" {
            STATE_COMPLETED
        } else {
            STATE_REJECTED
        };
        let enc = veles_core::pool::encode_reply(&reply)
            .unwrap_or_else(|_| "pool reply encode failed".into());
        let peer = format!("invite:{invited}");
        self.tasks.put(TaskRec {
            id: task_id.clone(),
            context_id: context_id.clone(),
            state: state.into(),
            peer: peer.clone(),
            inbound_text: "[pool join]".into(),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "pool_join_rpc", "task_id": task_id, "peer": peer,
            "state": state, "error": reply["error"],
        }));
        let task = build_task(&task_id, &context_id, state, &enc);
        (
            StatusCode::OK,
            Json(jsonrpc_result(&id, send_message_response(task))),
        )
    }

    /// One session-mail ask from `peer` (`veles_core::mailnet`): send,
    /// check or list, answered inline.
    async fn receive_session_mail(
        self: &Arc<Self>,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let cfg = veles_core::config::load().unwrap_or_default();
        let mut op = String::new();
        let reply = match veles_core::mailnet::decode_ask(raw) {
            Err(e) => json!({"status": "refused", "error": format!("bad ask: {e:#}")}),
            Ok(ask) => {
                op = ask.op.clone();
                veles_core::mailnet::receive(
                    &self.store,
                    &cfg.cluster.task_peers,
                    peer,
                    &peer_principal(peer),
                    ask,
                )
                .await
            }
        };
        let state = match reply["status"].as_str() {
            Some("ok") => STATE_COMPLETED,
            Some("error") => STATE_FAILED,
            _ => STATE_REJECTED,
        };
        let enc = veles_core::mailnet::encode_reply(&reply)
            .unwrap_or_else(|_| "session mail reply encode failed".into());
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[session mail {op}]"),
            reply_text: enc.chars().take(2000).collect(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "session_mail", "task_id": task_id, "context_id": context_id,
            "peer": peer, "op": op, "state": state,
            "mail": reply["id"], "error": reply["error"],
        }));
        build_task(task_id, context_id, state, &enc)
    }

    /// Receive one migration bundle: gate → decode → verify the
    /// addressee → adopt in ONE transaction → answer `adopted …` or a
    /// named refusal. The answer shape is the wire contract
    /// `migration::migrate` parses ("adopted " prefix), so it is
    /// pinned, not prose.
    fn receive_migration(&self, task_id: &str, context_id: &str, peer: &str, raw: &str) -> Value {
        let allowed = veles_core::config::load()
            .map(|c| c.cluster.migrate_peers)
            .unwrap_or_default();
        let me = self_node_id(&self.store.lock_ok());
        let (state, reply) = if !allowed.iter().any(|p| p == peer) {
            (
                STATE_REJECTED,
                format!("migration refused: '{peer}' is not in [cluster] migrate_peers"),
            )
        } else {
            match veles_core::migration::decode_bundle(raw) {
                Err(e) => (
                    STATE_REJECTED,
                    format!("migration refused: bad bundle: {e:#}"),
                ),
                Ok(bundle) => {
                    if me.is_empty() {
                        (
                            STATE_REJECTED,
                            "migration refused: this node has no identity (no node.toml) — \
                             it cannot be an adoption target"
                                .to_string(),
                        )
                    } else if bundle.to_node != me {
                        // The bundle names its addressee; adopting
                        // somebody else's migration would move a
                        // session neither operator asked to move here.
                        (
                            STATE_REJECTED,
                            format!(
                                "migration refused: the bundle is addressed to node '{}' — \
                                 this node is '{me}'",
                                bundle.to_node
                            ),
                        )
                    } else {
                        let store =
                            veles_core::store::lock_timed(&self.store, "a2a::migration_adopt");
                        match veles_core::migration::adopt(&store, &me, &bundle) {
                            Ok(rep) => {
                                let mut line = format!(
                                    "adopted {} (+{} subtree session(s), {} messages, \
                                     {} checkpoints, {} queued — held until `/queue run` \
                                     there) from {} ({})",
                                    bundle
                                        .sessions
                                        .first()
                                        .map(|s| s.id.as_str())
                                        .unwrap_or("?"),
                                    rep.sessions.saturating_sub(1),
                                    rep.messages,
                                    rep.checkpoints,
                                    rep.queue,
                                    bundle.from_name,
                                    bundle.from_node,
                                );
                                if !rep.missing_blobs.is_empty() {
                                    line.push_str(&format!(
                                        "; {} checkpoint blob(s) missing and named: {}",
                                        rep.missing_blobs.len(),
                                        rep.missing_blobs.join("; ")
                                    ));
                                }
                                if !rep.already_live.is_empty() {
                                    line.push_str(&format!(
                                        "; already live here (idempotent re-push, history \
                                         untouched): {}",
                                        rep.already_live.join(", ")
                                    ));
                                }
                                (STATE_COMPLETED, line)
                            }
                            Err(e) => (STATE_FAILED, format!("adoption failed: {e:#}")),
                        }
                    }
                }
            }
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: format!("[session migration, {} bytes]", raw.len()),
            reply_text: reply.chars().take(2000).collect(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "session_migration", "task_id": task_id,
            "peer": peer, "state": state, "bytes": raw.len(),
        }));
        build_task(task_id, context_id, state, &reply)
    }

    /// Answer a migration query from THIS store: `adopted <id> <node>`
    /// or `not-adopted <id>`. Gated by migrate_peers (only a migration
    /// peer has a migration to ask about). Deliberately store-read,
    /// never memory: the answer must be true for the session's WHOLE
    /// subtree root, which is what the source asked about.
    fn receive_migration_query(
        &self,
        task_id: &str,
        context_id: &str,
        peer: &str,
        raw: &str,
    ) -> Value {
        let allowed = veles_core::config::load()
            .map(|c| c.cluster.migrate_peers)
            .unwrap_or_default();
        let (state, reply) = if !allowed.iter().any(|p| p == peer) {
            (
                STATE_REJECTED,
                format!("migration query refused: '{peer}' is not in [cluster] migrate_peers"),
            )
        } else {
            match veles_core::migration::decode_query(raw) {
                Err(e) => (STATE_REJECTED, format!("bad query: {e:#}")),
                Ok((sid, _from)) => {
                    let store = veles_core::store::lock_timed(&self.store, "a2a::migration_query");
                    match veles_core::migration::encode_query_answer(&store, &sid) {
                        Ok(answer) => (STATE_COMPLETED, answer),
                        Err(e) => (STATE_FAILED, format!("query failed: {e:#}")),
                    }
                }
            }
        };
        self.tasks.put(TaskRec {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            state: state.into(),
            peer: peer.to_string(),
            inbound_text: raw.chars().take(200).collect(),
            reply_text: reply.chars().take(2000).collect(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        veles_core::a2a::audit_log(&json!({
            "event": "migration_query", "task_id": task_id,
            "peer": peer, "state": state,
        }));
        build_task(task_id, context_id, state, &reply)
    }
}

/// This node's registry id, read WITHOUT minting.
fn self_node_id(_store: &veles_core::store::Store) -> String {
    veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id)
        .unwrap_or_default()
}

/// Which node id an authenticated peer name is bound to: the node its
/// latest gossip observation was about, never this node's own row (see
/// `Store::node_bound_to_peer`). No binding yet = None — the peer's
/// registry rows must arrive first.
fn peer_node_binding(store: &veles_core::store::Store, peer: &str) -> Option<String> {
    store
        .node_bound_to_peer(peer, &self_node_id(store))
        .ok()
        .flatten()
}

/// A2A conversation persistence — outside the session DB, survives
/// compaction/restarts (hermes #11025 requirement). JSONL per context.
fn persist_conversation(context_id: &str, who: &str, dir: &str, text: &str) {
    let path = veles_core::config::veles_home()
        .join("a2a_conversations")
        .join(format!("{context_id}.jsonl"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = json!({
        "ts": veles_core::a2a::now_iso(),
        "who": who,
        "direction": dir,
        "text": text,
    });
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()));
}

async fn agent_card(State(srv): State<Arc<A2AServer>>) -> impl IntoResponse {
    let host = resolve_bind_host(&srv.cfg);
    let port = default_port(&srv.cfg);
    let url = format!("http://{host}:{port}/");
    let auth_required =
        !veles_core::a2a::bearer_token().is_empty() || !veles_core::a2a::peer_tokens().is_empty();
    // Per node, from what it has (PLAN-phone-userland.md T2): resolved
    // once per process, off the async runtime — the GPU probe is a
    // subprocess with a deadline, and a card is fetched often.
    static CAPS: tokio::sync::OnceCell<veles_core::node::NodeCaps> =
        tokio::sync::OnceCell::const_new();
    let node = srv.cfg.node.clone();
    let caps = CAPS
        .get_or_init(|| async move {
            veles_core::rt::spawn_blocking(move || veles_core::node::NodeCaps::resolve(&node))
                .await
                .unwrap_or_else(|_| veles_core::node::NodeCaps::unprobed(&srv.cfg.node))
        })
        .await;
    let skills = veles_core::a2a::skills_for(caps);
    Json(build_agent_card(
        "veles",
        &url,
        "Veles Agent — a general-purpose agent reachable over A2A.",
        &skills,
        auth_required,
    ))
}

/// The principal one authenticated A2A peer acts as.
///
/// It used to be a hard-coded `Role::Trusted`, which was a big thing to
/// hand out by default: trusted carries shell, file-write, delegate,
/// vision, tts and — since the media round — `video-gen`, i.e. the
/// ability to spend about 1200 credits a clip. USER, 2026-09-11, on
/// bridging slopdrop into veles: *"For calling into veles, I think User
/// level permissions would be enough."*
///
/// So the role comes from the ACL, keyed on the peer's identity —
/// `[acl.roles]` with `"a2a:slopdrop" = "user"`, and every other rule
/// shape that can name an identity works the same way.
///
/// …and `user` is also the answer when no rule names them. That is
/// `resolve_ctx_or` rather than plain `resolve_ctx` on purpose: an A2A
/// peer authenticated with a per-peer token the operator minted, so
/// "unlisted" here means "credential issued, role unstated" and not "a
/// stranger walked in". Falling to `default_role` would be right for a
/// stranger and would break every existing peer on an ACL-configured
/// install.
///
/// This LOWERS the default for peers that were relying on the old
/// hard-coded trusted — deliberately. An operator who wants it back
/// writes one line, and now has to mean it.
fn peer_principal(peer: &str) -> veles_core::acl::Principal {
    let id = format!("a2a:{peer}");
    let ctx = veles_core::acl::SenderContext {
        platform: Some("a2a".into()),
        direct: true,
        ..Default::default()
    };
    veles_core::acl::Principal::resolve_ctx_or(
        Some(&id),
        &ctx,
        &veles_core::acl::live_config(),
        veles_core::acl::Role::User,
    )
}

async fn rpc(
    State(srv): State<Arc<A2AServer>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let client_ip = addr.ip().to_string();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Card fetches are the one unauthenticated route; RPC is not.
    let Some(identity) = authenticate(auth.as_deref(), &client_ip) else {
        // Except a node redeeming its pool invite: its bearer is the
        // invite's own until this call registers it (veles_core::pool).
        if let Some(invited) = veles_core::pool::invite_bearer(auth.as_deref()) {
            if !srv.rate.allow(&format!("invite:{invited}")) {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(jsonrpc_error(
                        &body["id"],
                        ERR_RATE_LIMITED,
                        "rate limit exceeded",
                    )),
                );
            }
            return srv
                .pool_join_rpc(&invited, auth.as_deref().unwrap_or(""), &body)
                .await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(jsonrpc_error(
                &body["id"],
                veles_core::a2a::ERR_UNAUTHORIZED,
                "unauthorized",
            )),
        );
    };
    if !srv.rate.allow(&identity) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(jsonrpc_error(
                &body["id"],
                ERR_RATE_LIMITED,
                "rate limit exceeded",
            )),
        );
    }
    if !is_trusted_peer(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(jsonrpc_error(
                &body["id"],
                ERR_UNTRUSTED,
                "peer not in trusted list",
            )),
        );
    }

    dispatch_method(srv, &identity, &client_ip, &body).await
}

/// The JSON-RPC method table, after authentication.
///
/// Split out of `rpc` so it can be tested: `rpc` extracts
/// `ConnectInfo<SocketAddr>`, which axum only supplies behind a real
/// listener, so every method below was reachable in tests only through a
/// socket. `tasks/cancel` was cosmetic for months partly because
/// nothing could reach it cheaply enough to assert on.
///
/// The auth, rate-limit and trusted-peer gates stay in `rpc`, ABOVE this
/// — a test calling this directly is testing the method, not bypassing a
/// gate that protects it.
async fn dispatch_method(
    srv: Arc<A2AServer>,
    identity: &str,
    client_ip: &str,
    body: &Value,
) -> (StatusCode, Json<Value>) {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body["method"].as_str().unwrap_or("");
    let params = body.get("params").cloned().unwrap_or(json!({}));

    match method {
        "message/send" => {
            let task = srv.run_task(&params, identity, client_ip).await;
            (
                StatusCode::OK,
                Json(jsonrpc_result(&id, send_message_response(task))),
            )
        }
        "message/stream" => (
            StatusCode::OK,
            Json(jsonrpc_error(
                &id,
                -32601,
                "streaming not advertised on this agent's card; use message/send",
            )),
        ),
        "tasks/get" => {
            let tid = params
                .get("id")
                .or(params.get("name"))
                .or(params.get("taskId"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            // Same ownership rule as tasks/cancel (C3): a peer sees
            // only its OWN tasks. Sender-minted remote-task ids are
            // known outside this node by construction, so an ungated
            // get is a cross-peer read of another peer's state.
            match srv.tasks.get(tid).filter(|rec| rec.peer == identity) {
                Some(rec) => {
                    let task = build_task(&rec.id, &rec.context_id, &rec.state, &rec.reply_text);
                    (
                        StatusCode::OK,
                        Json(jsonrpc_result(&id, json!({"task": task}))),
                    )
                }
                None => (
                    StatusCode::OK,
                    Json(jsonrpc_error(&id, ERR_TASK_NOT_FOUND, "task not found")),
                ),
            }
        }
        "tasks/list" => {
            // Peer-scoped like get/cancel (C3): a peer lists only the
            // tasks IT created, not the whole node's task table.
            let items: Vec<Value> = srv
                .tasks
                .list(50)
                .iter()
                .filter(|r| r.peer == identity)
                .map(|r| {
                    json!({
                        "id": r.id,
                        "contextId": r.context_id,
                        "status": {"state": r.state, "timestamp": r.created},
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(jsonrpc_result(&id, json!({"tasks": items}))),
            )
        }
        "tasks/cancel" => {
            let tid = params
                .get("id")
                .or(params.get("name"))
                .or(params.get("taskId"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            // C3: ownership gate. A task belongs to the peer that
            // created it (`rec.peer` is the authenticated identity, so
            // this holds for every record, not just remote tasks).
            // Without it any authenticated peer could stop another
            // peer's running work by id — and remote task ids are now
            // SENDER-MINTED, i.e. known outside this node by
            // construction. Answered as "not found" rather than
            // "forbidden": existence of a task is not the asker's
            // business.
            match srv.tasks.get(tid).filter(|rec| rec.peer == identity) {
                Some(rec) => {
                    let terminal = matches!(
                        rec.state.as_str(),
                        STATE_COMPLETED
                            | STATE_FAILED
                            | STATE_CANCELED
                            | STATE_REJECTED
                            | STATE_INPUT_REQUIRED
                    );
                    if terminal {
                        (
                            StatusCode::OK,
                            Json(jsonrpc_error(
                                &id,
                                ERR_TASK_NOT_CANCELABLE,
                                "task already in terminal state",
                            )),
                        )
                    } else {
                        // Flip the token BEFORE writing the state, so the
                        // state this reports is true at the moment it is
                        // reported. The old order wrote "canceled" and
                        // left the turn running, which is the failure
                        // mode a cancel must not have. For a remote task
                        // the same token is threaded into the child's
                        // kill loop (C3), so the child process dies too.
                        rec.cancel.cancel();
                        srv.tasks.set_state(tid, STATE_CANCELED);
                        srv.turns.reset(&rec.context_id);
                        let task = build_task(&rec.id, &rec.context_id, STATE_CANCELED, "");
                        (
                            StatusCode::OK,
                            Json(jsonrpc_result(&id, json!({"task": task}))),
                        )
                    }
                }
                None => (
                    StatusCode::OK,
                    Json(jsonrpc_error(&id, ERR_TASK_NOT_FOUND, "task not found")),
                ),
            }
        }
        other => (
            StatusCode::OK,
            Json(jsonrpc_error(
                &id,
                -32601,
                &format!("method not found: {other}"),
            )),
        ),
    }
}

/// `[cluster] memsync_auto`, the receiving half: apply the inbox now
/// (oldest first, so the chunks of one push land in order). A conflict
/// stays in the inbox for a person — logged here, listed by `veles
/// memory inbox`, reported again by `veles memory apply --inbox`.
/// Also the sync loop's pre-decision drain, shared with the receive path.
pub(crate) async fn apply_memsync_inbox() -> String {
    let globs = veles_core::config::load()
        .map(|c| c.cluster.local_memory_globs)
        .unwrap_or_default();
    let done = veles_core::rt::spawn_blocking(move || {
        let mem = veles_core::memory::MemoryStore::new();
        veles_core::memsync::apply_inbox(&mem, &globs)
    })
    .await;
    match done {
        Ok(Ok(reports)) => {
            let held: Vec<String> = reports
                .iter()
                .filter(|(_, r)| !r.is_clean())
                .map(|(name, r)| format!("{name}: {}", r.summarize()))
                .collect();
            for h in &held {
                tracing::warn!(
                    "memsync: a conflict is held in the inbox — `veles memory apply --inbox` \
                     reports it: {h}"
                );
            }
            if held.is_empty() {
                format!("applied ({} offer(s))", reports.len())
            } else {
                format!(
                    "applied; {} offer(s) with a conflict held in the inbox",
                    held.len()
                )
            }
        }
        Ok(Err(e)) => {
            tracing::warn!("memsync: applying the inbox failed: {e:#}");
            format!("stored, not applied: {e:#}")
        }
        Err(e) => format!("stored, not applied: {e}"),
    }
}

/// The largest JSON-RPC request the A2A endpoint reads.
pub const A2A_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Serve until the future is dropped. Call from inside the gateway's
/// tokio runtime.
pub async fn serve(srv: Arc<A2AServer>) -> Result<()> {
    let host = resolve_bind_host(&srv.cfg);
    let port = default_port(&srv.cfg);
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(agent_card))
        .route("/.well-known/agent.json", get(agent_card)) // legacy path
        .route("/", post(rpc))
        // Sync offers ride this endpoint. axum's default (2 MB) refused
        // a 500-row storesync chunk of large rows with a plain-text 413
        // (2026-10-04); senders now cap chunks at 768 KB of rows, and
        // this room is for senders that do not yet.
        .layer(axum::extract::DefaultBodyLimit::max(A2A_BODY_LIMIT))
        .with_state(srv);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    tracing::info!("A2A server listening on http://{addr}/ (card: /.well-known/agent-card.json)");
    let listener = bind_listener(addr)?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Bind the A2A listener.
///
/// A specific address — the WireGuard one a node is reached at — is
/// bound with `IP_FREEBIND` on Linux and Android. The gateway can start
/// before the tunnel does (the phone at boot, the laptop before wg-quick),
/// and a plain bind to an address the machine does not have yet fails
/// with EADDRNOTAVAIL, which left A2A down until the next restart. With
/// it the listener is in place and connections arrive once the address
/// exists. Loopback and the wildcard always exist and bind as before.
fn bind_listener(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let SocketAddr::V4(v4) = addr {
        if !v4.ip().is_unspecified() && !v4.ip().is_loopback() {
            return bind_freebind_v4(v4);
        }
    }
    let l = std::net::TcpListener::bind(addr)?;
    l.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(l)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_freebind_v4(addr: std::net::SocketAddrV4) -> std::io::Result<tokio::net::TcpListener> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let check = |rc: libc::c_int| {
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    };
    // SAFETY: a fresh socket owned by `fd` (closed on every early
    // return), plain integer options, and a sockaddr_in built in full.
    unsafe {
        let raw = check(libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        ))?;
        let fd = OwnedFd::from_raw_fd(raw);
        let one: libc::c_int = 1;
        let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        for (level, opt) in [
            (libc::SOL_SOCKET, libc::SO_REUSEADDR),
            (libc::IPPROTO_IP, libc::IP_FREEBIND),
        ] {
            check(libc::setsockopt(
                fd.as_raw_fd(),
                level,
                opt,
                &one as *const libc::c_int as *const libc::c_void,
                size,
            ))?;
        }
        let sa = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: addr.port().to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(addr.ip().octets()),
            },
            sin_zero: [0; 8],
        };
        check(libc::bind(
            fd.as_raw_fd(),
            &sa as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ))?;
        check(libc::listen(fd.as_raw_fd(), 1024))?;
        tokio::net::TcpListener::from_std(std::net::TcpListener::from(fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A2A on an address this machine does not have (yet): the bind
    /// succeeds, where a plain one fails — the gateway that starts
    /// before its WireGuard tunnel keeps its A2A.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn a2a_binds_an_address_that_is_not_up_yet() {
        // 192.0.2.0/24 is TEST-NET-1: never assigned to a real host.
        let addr: SocketAddr = "192.0.2.77:0".parse().unwrap();
        assert!(
            std::net::TcpListener::bind(addr).is_err(),
            "the premise: a plain bind refuses an address the host lacks"
        );
        let l = bind_listener(addr).expect("IP_FREEBIND binds it");
        assert_eq!(l.local_addr().unwrap().ip().to_string(), "192.0.2.77");
        // Loopback still binds the plain way, and serves.
        let lo = bind_listener("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = lo.local_addr().unwrap().port();
        let (_c, _s) = tokio::join!(
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
            lo.accept()
        );
    }

    pub(super) fn scratch_server() -> (
        tempfile::TempDir,
        Arc<A2AServer>,
        veles_core::testenv::EnvGuard,
    ) {
        let (home, env) = veles_core::testenv::scratch_home();
        // Agent::new resolves the API key from VELES_HOME/.env; an empty
        // key makes ProviderClient::new fail and the route build 500s.
        std::fs::write(home.path().join(".env"), "VELES_API_KEY=test-key\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        // Dead local port: no turn is ever run in these tests; they pin
        // the request-handling shell, not the LLM round trip.
        let cfg = Arc::new(VelesConfig {
            model: veles_core::config::ModelSection {
                provider: Some("test-provider".into()),
                default: Some("test-model".into()),
                ..Default::default()
            },
            custom_providers: vec![veles_core::config::ProviderEntry {
                name: "test-provider".into(),
                base_url: "http://127.0.0.1:0/v1".into(),
                api_mode: "chat_completions".into(),
                models: veles_core::config::ModelsSpec::List(vec!["test-model".into()]),
                ..Default::default()
            }],
            ..Default::default()
        });
        let hooks = HookBus::new(None);
        (home, A2AServer::new(cfg, store, hooks), env)
    }

    fn offer_text() -> String {
        let (_d, store) = {
            let d = tempfile::tempdir().unwrap();
            let s = veles_core::memory::MemoryStore::at(d.path().to_path_buf());
            (d, s)
        };
        store
            .write_file("system/user.md", "reach me at someone@example.org")
            .unwrap();
        let offer = veles_core::memsync::build_offer(&store, "node-a", "laptop", 0).unwrap();
        veles_core::memsync::encode_message(&offer).unwrap()
    }

    /// A memory sync offer is data for the operator, not a message for
    /// the agent. The first version sent it as ordinary conversation, so
    /// the peer ran an LLM turn over the whole memory (~700k tokens on
    /// the live store) and nothing could apply it. Now: stored in the
    /// inbox, COMPLETED, and no session — so no turn — ever exists.
    #[tokio::test]
    async fn a_memsync_offer_is_stored_not_run_as_a_turn() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nmemsync_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let params = json!({
            "message": {"role": "user", "contextId": "ctx-memsync",
                        "parts": [{"kind": "text", "text": offer_text()}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        assert!(
            srv.sessions.lock_ok().is_empty(),
            "no session, therefore no turn, for an offer"
        );
        let pending =
            veles_core::memsync::inbox_pending(&veles_core::memsync::inbox_dir()).unwrap();
        assert_eq!(pending.len(), 1, "exactly one stored offer");
        let entry = veles_core::memsync::inbox_read(&pending[0]).unwrap();
        assert_eq!(entry.peer, "peer-x");
        assert_eq!(
            entry.offer.revisions[0].content, "reach me at someone@example.org",
            "the content arrives byte-identical — not redacted in transit"
        );
    }

    /// `[cluster] memsync_auto`: the offer is applied on receipt — the
    /// path lands in this node's memory and the inbox entry moves to
    /// `applied/`.
    #[tokio::test]
    async fn a_memsync_offer_is_applied_on_receipt_with_memsync_auto() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nmemsync_peers = [\"peer-x\"]\nmemsync_auto = true\n",
        )
        .unwrap();
        let params = json!({
            "message": {"role": "user", "contextId": "ctx-memsync-auto",
                        "parts": [{"kind": "text", "text": offer_text()}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        assert!(task.to_string().contains("applied"), "{task}");
        let dir = veles_core::memsync::inbox_dir();
        assert!(veles_core::memsync::inbox_pending(&dir).unwrap().is_empty());
        assert_eq!(std::fs::read_dir(dir.join("applied")).unwrap().count(), 1);
        let mem = veles_core::memory::MemoryStore::new();
        assert_eq!(
            mem.latest_content("system/user.md").unwrap(),
            "reach me at someone@example.org"
        );
    }

    /// `[cluster] configsync_auto` (the default): a config offer from a
    /// sync peer is applied on receipt — and a security-section row in
    /// it waits in the inbox for the owner.
    #[tokio::test]
    async fn a_config_offer_applies_on_receipt_except_security() {
        let (home, srv, _env) = scratch_server();
        let cfg_path = home.path().join("config.toml");
        std::fs::write(&cfg_path, "[cluster]\nmemsync_peers = [\"peer-x\"]\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&cfg_path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let at = veles_core::configsync::now_stamp();
        let row = |seq, key: &str, value: &str| veles_core::configsync::ConfigLogRow {
            node: "peer-node".into(),
            node_seq: seq,
            at: at.clone(),
            key: key.into(),
            value: value.into(),
        };
        let offer = veles_core::configsync::ConfigSyncOffer {
            from_node: "peer-node".into(),
            from_name: "laptop".into(),
            since_seq: 0,
            rows: vec![
                row(1, "agent.reasoning_effort", "\"low\""),
                row(2, "acl.default_role", "\"user\""),
            ],
        };
        let text = veles_core::configsync::encode_message(&offer).unwrap();
        let params = json!({
            "message": {"role": "user", "contextId": "ctx-configsync-auto",
                        "parts": [{"kind": "text", "text": text}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        assert!(task.to_string().contains("wait for the owner"), "{task}");
        let after = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(after.contains("reasoning_effort = \"low\""), "{after}");
        assert!(!after.contains("default_role"), "{after}");
        assert_eq!(veles_core::configsync::inbox_waiting().unwrap().len(), 1);
    }

    /// `[cluster] configsync_auto` carries the vault: an offer from a
    /// sync peer merges on receipt into the vault this node has, and
    /// the rows that peer holds are noted, so the loop never sends them
    /// back to it.
    #[tokio::test]
    async fn a_vault_offer_merges_on_receipt_and_is_noted_as_the_senders() {
        use veles_core::vault::{self, Kind, Policy, Vault};
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nmemsync_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        // The peer's vault (root), and this node admitted to it.
        let peer_dir = tempfile::tempdir().unwrap();
        let mut root = Vault::open_or_create(peer_dir.path(), "peer-node", "laptop").unwrap();
        root.init().unwrap();
        let mut here = Vault::open_or_create_local().unwrap();
        let req = here.join_request();
        root.admit(&req, &req.fingerprint()).unwrap();
        let fp = root.keys.fingerprint();
        let file = root.file.clone();
        here.update(|v| v.apply(&file, Some(&fp))).unwrap();
        // A write there, offered here.
        root.set(
            "SYNC_PROBE_KEY",
            "probe-value-123",
            Policy::default_for(Kind::User, vec![]),
        )
        .unwrap();
        let msg = vault::Message::Offer {
            from_node: "peer-node".into(),
            from_name: "laptop".into(),
            file: root.file.clone(),
        };
        let params = json!({
            "message": {"role": "user", "contextId": "ctx-vault-auto",
                        "parts": [{"kind": "text", "text": vault::encode_message(&msg).unwrap()}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        assert!(task.to_string().contains("merged"), "{task}");
        let after = Vault::open_local().unwrap();
        assert_eq!(
            after.get("SYNC_PROBE_KEY").unwrap().unwrap().value,
            "probe-value-123"
        );
        assert!(vault::inbox_pending(&vault::inbox_dir())
            .unwrap()
            .is_empty());
        assert_eq!(
            srv.store.lock_ok().meta_get(&vault::peer_has_key("peer-x")),
            Some(vault::rows_digest(&root.file)),
            "what the sender holds is noted"
        );
    }

    /// An ask relay is answered inline for a `task_peers` peer — the
    /// question goes to this node's sink (its home chat), the owner's
    /// answer comes back framed — and refused, untouched, for anyone else.
    #[tokio::test]
    async fn an_ask_relay_is_put_to_the_owner_here_for_a_task_peer() {
        struct Approve;
        #[async_trait::async_trait]
        impl veles_core::ask::Asker for Approve {
            async fn ask(&self, _r: veles_core::ask::AskRequest) -> veles_core::ask::AskAnswer {
                veles_core::ask::AskAnswer::choice("approve")
            }
        }
        let (home, srv, _env) = scratch_server();
        veles_core::askrelay::set_sink(Some(std::sync::Arc::new(Approve)));
        let relay = veles_core::askrelay::encode(&veles_core::askrelay::AskRelayWire {
            op: "ask".into(),
            from_node: "38a5e4dd".into(),
            from_name: "laptop".into(),
            id: "ask1-aa".into(),
            ask: Some(veles_core::ask::AskRequest::new(
                "s1",
                veles_core::ask::AskKind::Approval,
                "run `ls`?",
                veles_core::ask::approval_choices(true),
            )),
            key: None,
        });
        let params = |ctx: &str| {
            json!({"message": {"role": "user", "contextId": ctx,
                               "parts": [{"kind": "text", "text": relay}]}})
        };
        let refused = srv
            .run_task(&params("ctx-relay-1"), "peer-x", "127.0.0.1:0")
            .await;
        assert_eq!(
            refused["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{refused}"
        );
        assert!(refused.to_string().contains("task_peers"), "{refused}");
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let task = srv
            .run_task(&params("ctx-relay-2"), "peer-x", "127.0.0.1:0")
            .await;
        veles_core::askrelay::set_sink(None);
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        let answer = veles_core::askrelay::decode_answer(&veles_core::a2a::extract_text(&task))
            .expect("a framed answer");
        assert_eq!(answer.selected, vec!["approve".to_string()]);
        assert!(srv.sessions.lock_ok().is_empty(), "no session, no turn");
    }

    /// …and only from a peer the operator named. Default config names
    /// nobody, so the offer is refused and nothing touches the disk.
    #[tokio::test]
    async fn a_memsync_offer_from_an_unnamed_peer_is_refused() {
        let (_home, srv, _env) = scratch_server();
        let params = json!({
            "message": {"role": "user",
                        "parts": [{"kind": "text", "text": offer_text()}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{task}"
        );
        assert!(task.to_string().contains("memsync_peers"), "{task}");
        assert!(srv.sessions.lock_ok().is_empty());
        assert!(
            veles_core::memsync::inbox_pending(&veles_core::memsync::inbox_dir())
                .unwrap()
                .is_empty()
        );
    }

    /// A2A is a connector, and it does NOT pass through the gateway —
    /// it holds its own `Agent` and calls `run_turn` itself. So the
    /// gateway's lockdown gate cannot see it and this one must.
    ///
    /// The refusal rides in the task record, because an RPC peer has
    /// nowhere else to read it, and the task is REJECTED rather than
    /// failed: nothing went wrong, the door is shut.
    #[tokio::test]
    async fn lockdown_rejects_an_a2a_task_before_the_turn() {
        let (_home, srv, _env) = scratch_server();
        veles_core::lockdown::engage(&srv.store, "tui:owner", "an incident").unwrap();
        let params = json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": "do a thing"}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "a locked-down agent must not run a peer's turn: {task}"
        );
        let text = task.to_string();
        assert!(
            text.contains("locked down"),
            "the peer is told what happened: {text}"
        );
        assert!(
            text.contains("local frontend"),
            "…and that it is not theirs to lift: {text}"
        );
        veles_core::lockdown::release(&srv.store).unwrap();
    }

    /// Until 2026-09-11 an A2A session had NO principal, and no
    /// principal is the owner surface: any authenticated peer ran an
    /// owner-equivalent agent that neither the media gate nor the F1
    /// read jail could bind. It was then bound to a hard-coded
    /// `Trusted`, which was still a lot to hand out by default —
    /// trusted carries shell, file-write, delegate and `video-gen`, i.e.
    /// about 1200 credits a clip. USER, 2026-09-11, bridging slopdrop
    /// in: *"For calling into veles, I think User level permissions
    /// would be enough."*
    ///
    /// So the role comes from the ACL now, and `user` is the answer when
    /// nothing names the peer. The turn itself fails against the dead
    /// test port; the binding precedes it, so this pins the binding
    /// without needing an LLM.
    #[tokio::test]
    async fn a_peer_session_is_a_user_not_the_owner_surface() {
        let (_home, srv, _env) = scratch_server();
        let params = json!({
            "message": {"role": "user",
                        "contextId": "ctx-principal-test",
                        "parts": [{"kind": "text", "text": "do a thing"}]}
        });
        let _ = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        let sess = srv.session_for("ctx-principal-test").unwrap();
        let agent = sess.agent.lock().await;
        let p = agent
            .principal()
            .expect("the session must carry the peer's principal");
        assert_eq!(
            p.role,
            veles_core::acl::Role::User,
            "not owner, not trusted"
        );
        assert_eq!(p.subject, "a2a:peer-x");
        assert_eq!(p.id.as_deref(), Some("a2a:peer-x"));
        assert!(
            p.caps.contains(&veles_core::acl::Cap::MediaGen),
            "a user peer may still generate media — that is a user cap"
        );
        assert!(
            !p.caps.contains(&veles_core::acl::Cap::VideoGen),
            "…but not the 1200-credit one, which starts at trusted"
        );
        assert!(
            !p.caps.contains(&veles_core::acl::Cap::All),
            "an authenticated peer is never the owner surface"
        );
    }

    /// …and the ACL decides, in both directions.
    ///
    /// The default is a floor for a credential the operator minted, not
    /// a ceiling: `[acl.roles]."a2a:slopdrop" = "trusted"` raises it and
    /// `"banned"` removes it, because an A2A identity is an identity
    /// like any other once it has a name.
    #[tokio::test]
    async fn an_acl_rule_decides_what_a_peer_is() {
        let (home, _srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[acl]\nowner = [\"irc:wrath\"]\ndefault_role = \"guest\"\n\n\
             [acl.roles]\n\"a2a:trustedpeer\" = \"trusted\"\n\"a2a:badpeer\" = \"banned\"\n",
        )
        .unwrap();
        veles_core::acl::invalidate_live_config();

        let named = peer_principal("trustedpeer");
        assert_eq!(
            named.role,
            veles_core::acl::Role::Trusted,
            "a rule raises it"
        );
        let banned = peer_principal("badpeer");
        assert_eq!(banned.role, veles_core::acl::Role::Banned, "and removes it");
        assert!(banned.caps.is_empty());

        // An unnamed peer stays `user` even though `default_role` is
        // guest: it authenticated with a token this operator issued, so
        // "unlisted" means "role unstated", not "a stranger walked in".
        let unnamed = peer_principal("someoneelse");
        assert_eq!(unnamed.role, veles_core::acl::Role::User);
        assert!(
            unnamed.via.as_deref().unwrap_or("").contains("no acl rule"),
            "and it says so: {:?}",
            unnamed.via
        );
    }

    /// `tasks/cancel` must STOP the turn, not relabel it.
    ///
    /// Until 2026-09-03 it set `state` to "canceled" and called
    /// `turns.reset`, and the turn kept running — a peer was told the
    /// work had stopped while it was still spending tokens. The load
    /// bearing property is that the token the running turn holds and the
    /// token the cancel handler flips are the SAME flag, which is why
    /// `CancelToken` (an `Arc<AtomicBool>`) is cloned rather than copied
    /// out of the record.
    #[tokio::test]
    async fn cancelling_a_task_reaches_the_turn_that_is_running() {
        let (_home, srv, _env) = scratch_server();
        let rec = TaskRec {
            id: "t-cancel".into(),
            context_id: "ctx".into(),
            state: STATE_WORKING.into(),
            peer: "peer".into(),
            inbound_text: "work".into(),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        };
        srv.tasks.put(rec);

        // What the running turn holds: a clone taken at turn start,
        // exactly as `send` does.
        let held_by_the_turn = srv.tasks.get("t-cancel").expect("task").cancel;
        assert!(
            !held_by_the_turn.is_cancelled(),
            "a fresh task is not cancelled"
        );

        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/cancel",
            "params": {"taskId": "t-cancel"}
        });
        let (code, Json(resp)) = dispatch_method(srv.clone(), "peer", "127.0.0.1:0", &body).await;
        assert_eq!(code, StatusCode::OK, "{resp}");
        assert!(resp["error"].is_null(), "cancel refused: {resp}");

        assert!(
            held_by_the_turn.is_cancelled(),
            "the turn's own token was never flipped — the cancel is cosmetic \
             again and the peer is being lied to"
        );
        assert_eq!(
            srv.tasks.get("t-cancel").expect("task").state,
            STATE_CANCELED,
            "and the reported state follows the token"
        );
    }

    /// A task already finished cannot be cancelled, and the attempt must
    /// not pretend otherwise.
    #[tokio::test]
    async fn a_terminal_task_refuses_cancellation() {
        let (_home, srv, _env) = scratch_server();
        srv.tasks.put(TaskRec {
            id: "t-done".into(),
            context_id: "ctx".into(),
            state: STATE_COMPLETED.into(),
            peer: "peer".into(),
            inbound_text: "work".into(),
            reply_text: "done".into(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/cancel",
            "params": {"taskId": "t-done"}
        });
        let (_code, Json(resp)) = dispatch_method(srv.clone(), "peer", "127.0.0.1:0", &body).await;
        assert_eq!(
            resp["error"]["code"], ERR_TASK_NOT_CANCELABLE,
            "a completed task is not cancelable: {resp}"
        );
        assert!(
            !srv.tasks.get("t-done").expect("task").cancel.is_cancelled(),
            "and nothing was flipped on the way out"
        );
    }

    #[test]
    fn default_port_prefers_env_then_config_then_9900() {
        // scratch_home repoints VELES_HOME so the dotenv lookup reads OUR
        // .env, not the developer's real one (which sets A2A_PORT=9910).
        let (home, _env) = veles_core::testenv::scratch_home();
        // The AMBIENT A2A_PORT must not shadow the dotenv case either —
        // a gateway machine exports it (env beats dotenv by design), and
        // the first assertion only holds when it starts unset. Put the
        // machine's value back on the way out.
        let prev_port = std::env::var("A2A_PORT").ok();
        std::env::remove_var("A2A_PORT");
        std::fs::write(home.path().join(".env"), "A2A_PORT=9911\n").unwrap();
        let mut cfg = VelesConfig::default();
        assert_eq!(default_port(&cfg), 9911, "dotenv A2A_PORT wins");
        // Env beats dotenv: set it explicitly and the .env value loses.
        std::env::set_var("A2A_PORT", "8123");
        assert_eq!(default_port(&cfg), 8123, "env var beats dotenv");
        std::env::remove_var("A2A_PORT");
        // Dotenv silenced (delete the scratch .env) → config extra.port.
        std::fs::remove_file(home.path().join(".env")).unwrap();
        cfg.platforms =
            serde_yaml::from_str("a2a:\n  enabled: true\n  extra:\n    port: 8643\n").unwrap();
        assert_eq!(
            default_port(&cfg),
            8643,
            "config extra.port when env+dotenv silent"
        );
        if let Some(p) = prev_port {
            std::env::set_var("A2A_PORT", p);
        }
    }

    #[test]
    fn a2a_enabled_defaults_off() {
        let mut cfg = VelesConfig::default();
        assert!(!a2a_enabled(&cfg));
        // Real YAML with a real newline — the r#"\n"# form writes a
        // literal backslash-n, which serde_yaml parses as a key named
        // "a2a:\n  enabled: true", and enabled lookups miss.
        cfg.platforms = serde_yaml::from_str("a2a:\n  enabled: true\n").unwrap();
        assert!(a2a_enabled(&cfg));
    }

    #[tokio::test]
    async fn rpc_without_connect_info_is_rejected_by_the_extractor() {
        let (_h, srv, _e) = scratch_server();
        // The rpc handler extracts ConnectInfo<SocketAddr>; axum only
        // provides that via into_make_service_with_connect_info — i.e.
        // behind a REAL listener. A request without it is rejected by
        // the extractor before the handler runs. That is worth pinning:
        // no caller can bypass the IP gate by omitting peer info.
        let app = Router::new().route("/", post(rpc)).with_state(srv.clone());
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": {"message": {"parts": [{"text": "hi"}]}}
        });
        let res = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

#[cfg(test)]
mod b3a_pins {
    use super::*;

    use super::tests::scratch_server;
    // The scratch_server fixture mints no node identity and no
    // arbiter, so lease asks on it land in the "not the arbiter"
    // refusal — which is exactly the pin for that arm.
    #[tokio::test]
    async fn lease_ask_refused_when_not_arbiter() {
        let (_home, srv, _env) = scratch_server();
        let ask = veles_core::gossip::encode_lease_ask("singleton:cron", "aaaa", true);
        let params = json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": ask}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{task}"
        );
        assert!(
            task.to_string().contains("not the arbiter"),
            "the refusal says why: {task}"
        );
        assert!(
            srv.sessions.lock_ok().is_empty(),
            "no session for a lease ask"
        );
    }

    /// On a node that IS the arbiter, a peer may only ask for the node
    /// id it is bound to (the gossip-recorded observation). An
    /// allowlisted peer asking for a FOREIGN node id is refused.
    #[tokio::test]
    async fn lease_ask_refuses_a_foreign_holder() {
        let (_home, srv, _env) = scratch_server();
        // Register nodes + bind peer-x to node A via an observation
        // (what gossip would have written), make node B this process's
        // identity… simpler: arbiter is a third id; we ARE the arbiter
        // by writing node.toml with the arbiter's id.
        let arbiter_id = "arb-0001";
        let node_a = "aaaa-1111";
        let node_b = "bbbb-2222";
        {
            let s = srv.store.lock().unwrap();
            s.node_register(
                arbiter_id,
                "vps",
                "h",
                "0",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
            s.node_register(
                node_a,
                "laptop",
                "h",
                "0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
            // peer-x is bound to node_a (the observation gossip records).
            s.node_observe(node_a, "127.0.0.1:0", "peer-x").unwrap();
        }
        std::fs::write(
            veles_core::config::config_dir().join("node.toml"),
            format!("id = \"{arbiter_id}\"\nname = \"vps\"\n"),
        )
        .unwrap();
        // The allowlist is read LIVE from the config file (adding a
        // peer needs no restart), so the scratch home needs a real one.
        std::fs::write(
            veles_core::config::config_dir().join("config.toml"),
            format!("[cluster]\narbiter = \"{arbiter_id}\"\nmemsync_peers = [\"peer-x\"]\n"),
        )
        .unwrap();
        // The server's own cfg names the arbiter too (the
        // "am I the arbiter" check reads it).
        let mut new_cfg = VelesConfig::default();
        new_cfg.cluster.arbiter = Some(arbiter_id.to_string());
        new_cfg.cluster.memsync_peers = vec!["peer-x".to_string()];
        let srv2 = A2AServer::new(Arc::new(new_cfg), srv.store.clone(), HookBus::new(None));

        // Honest ask (own node id): granted.
        let ask_own = veles_core::gossip::encode_lease_ask("singleton:cron", node_a, true);
        let params = json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": ask_own}]}
        });
        let task = srv2.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "bound peer asking for its own node: {task}"
        );

        // Foreign ask (another node's id): refused.
        let ask_foreign = veles_core::gossip::encode_lease_ask("singleton:cron", node_b, true);
        let params = json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": ask_foreign}]}
        });
        let task = srv2.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "a peer must not grab another node's lease: {task}"
        );
        assert!(
            task.to_string().contains("bound to node"),
            "the refusal names the binding: {task}"
        );

        // Gossip running BOTH ways: the arbiter's own exchange with
        // peer-x comes back with an echo, recorded as "peer-x saw the
        // ARBITER at <addr>" — later than peer-x's own claim. That says
        // where peer-x saw us, not who peer-x is: its honest ask must
        // still be served. (B3a bound peer-x to the arbiter here.)
        {
            let s = srv2.store.lock().unwrap();
            s.node_observe(arbiter_id, "10.0.30.1:9900", "peer-x")
                .unwrap();
        }
        let params = json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": ask_own}]}
        });
        let task = srv2.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "the echo of our own gossip must not rebind the peer: {task}"
        );
    }

    /// A lease ask whose payload does not match its sha is refused —
    /// B3a decoded asks without checking it (answers were checked).
    #[tokio::test]
    async fn a_tampered_lease_ask_is_refused() {
        let ask = veles_core::gossip::encode_lease_ask("singleton:cron", "aaaa", true);
        let tampered = ask.replace("sha256:", "sha256:00");
        assert!(veles_core::gossip::decode_lease_ask(&tampered).is_err());
        assert!(veles_core::gossip::decode_lease_ask(&ask).is_ok());
    }

    // ── B4: remote delegate over the cluster link ──────────────────

    pub(super) fn task_ask(ask: &veles_core::remotetask::RemoteTaskAsk) -> Value {
        json!({
            "message": {"role": "user", "parts": [{"kind": "text",
                "text": veles_core::remotetask::encode_ask(ask).unwrap()}]}
        })
    }

    pub(super) fn base_ask() -> veles_core::remotetask::RemoteTaskAsk {
        veles_core::remotetask::RemoteTaskAsk {
            from_node: "aaaa-1111".into(),
            goal: "count files in /tmp".into(),
            context: "a peer placed this here".into(),
            model: None,
            provider: None,
            reasoning: None,
            principal: None,
            wire_task_id: None,
            ticket: None,
        }
    }

    /// A remote task from an ALLOWLISTED peer runs the same child
    /// runner a local delegate uses — observed through VELES_BINARY —
    /// and the fenced reply carries the verdict plus the LOCAL child
    /// session id. No agent session is created for the wire message
    /// itself: the ask is a cluster instruction, not a turn.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_remote_task_runs_a_child_and_answers_inline() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        // A fake child: records its argv, prints a summary, exits 0.
        let log = home.path().join("argv.log");
        let fake = home.path().join("fake-veles");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> {}\necho ran-on-b\n",
                log.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);

        let task = srv
            .run_task(&task_ask(&base_ask()), "peer-x", "127.0.0.1:0")
            .await;
        std::env::remove_var("VELES_BINARY");

        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        assert!(
            srv.sessions.lock_ok().is_empty(),
            "the ask itself is not a turn: {task}"
        );
        // The child ran with the goal in its prompt.
        let argv = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            argv.contains("count files in /tmp"),
            "the goal reaches the child process: {argv}"
        );
        assert!(
            argv.contains("a peer placed this here"),
            "and its context: {argv}"
        );
        // The reply decodes and names the child session — read exactly
        // the way `run_on_peer` reads it (extract_text on the task).
        let text = veles_core::a2a::extract_text(&task);
        assert!(veles_core::remotetask::is_remote_reply(&text), "{text}");
        let v = veles_core::remotetask::decode_reply(&text).unwrap();
        assert_eq!(v["status"], "ok", "{v}");
        assert_eq!(v["summary"], "ran-on-b", "{v}");
        assert!(
            v["remote_child_session"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "the LOCAL child session is named so it is attachable here: {v}"
        );
        // The child session row exists on THIS node — run_remote_child
        // mints it against VELES_HOME/state.db (the store a gateway
        // process opens), titled `remote: …` and parented to the
        // pseudo-session a2a-remote: remote work is reviewable where
        // it RAN. (Not srv.store — the fixture's store is a separate
        // tempdir; the runner and a real gateway agree on veles_home.)
        let cs = v["remote_child_session"].as_str().unwrap();
        let home_store =
            veles_core::store::Store::open(&veles_core::config::veles_home().join("state.db"))
                .unwrap();
        assert!(
            home_store.session_exists(cs).unwrap(),
            "the named session row exists on this node: {v}"
        );
        assert_eq!(
            home_store.session_parent(cs).unwrap().as_deref(),
            Some("a2a-remote"),
            "the remote child is parented to the a2a pseudo-session"
        );
        let titled = home_store
            .session_meta(64)
            .unwrap()
            .into_iter()
            .find(|m| m.id == cs)
            .map(|m| m.title)
            .unwrap_or_default();
        assert!(
            titled.starts_with("remote:"),
            "the remote child is titled as one: {titled:?}"
        );
    }

    /// The allowlist is the whole gate: an unlisted peer's remote task
    /// is REJECTED before any child spawns — a bearer token alone does
    /// not buy compute on this node.
    #[tokio::test]
    async fn a_remote_task_from_an_unlisted_peer_is_refused() {
        let (_home, srv, _env) = scratch_server();
        let task = srv
            .run_task(&task_ask(&base_ask()), "peer-x", "127.0.0.1:0")
            .await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{task}"
        );
        assert!(
            task.to_string().contains("task_peers"),
            "the refusal names the list: {task}"
        );
        assert!(srv.sessions.lock_ok().is_empty());
    }

    /// Being a SYNC peer does not buy compute (USER, 2026-09-28): B4
    /// gated remote work on memsync_peers, the list for inbox offers the
    /// operator applies by hand. Remote work has its own list.
    #[tokio::test]
    async fn a_sync_peer_is_not_a_task_peer() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nmemsync_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let task = srv
            .run_task(&task_ask(&base_ask()), "peer-x", "127.0.0.1:0")
            .await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "a memsync peer must not run work here: {task}"
        );
        assert!(task.to_string().contains("task_peers"), "{task}");
    }

    /// The remote child runs under the CALLER's principal, carried in
    /// the ask — B4 ran it principal-free, i.e. as OWNER on this node,
    /// so a Trusted caller delegating here got Cap::All — and at most at
    /// the User level without a ticket (D2): an owner surface's
    /// delegation is capped too, because a compromised node must not get
    /// a host shell here by delegating. A principal that does not parse
    /// is refused before any child starts, never widened to "none".
    #[cfg(unix)]
    #[tokio::test]
    async fn a_remote_child_runs_under_the_callers_principal() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let ran = home.path().join("ran.log");
        let fake = home.path().join("fake-veles");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho ran >> {}\nprintf 'PRINCIPAL=[%s]\\n' \"$VELES_SESSION_PRINCIPAL\"\n",
                ran.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);
        let trusted = veles_core::acl::Principal {
            id: Some("irc:someone".into()),
            subject: "user:someone".into(),
            role: veles_core::acl::Role::Trusted,
            caps: veles_core::acl::Role::Trusted.default_caps(),
            rule_context_limit: None,
            mask: None,
            via: Some("test".into()),
            ticket_role: None,
        };
        let verdict = |task: &Value| {
            veles_core::remotetask::decode_reply(&veles_core::a2a::extract_text(task)).unwrap()
        };

        let mut ask = base_ask();
        ask.principal = Some(serde_json::to_string(&trusted).unwrap());
        let task = srv.run_task(&task_ask(&ask), "peer-x", "127.0.0.1:0").await;
        let v = verdict(&task);
        assert!(
            v.to_string().contains("user:someone"),
            "the child carries the caller's principal: {v}"
        );
        assert!(
            v.to_string().contains("peer:peer-x/user:someone"),
            "a trusted caller runs here capped, as the peer's: {v}"
        );

        // An owner caller (no principal) is capped at user as well — not
        // this node's owner.
        let task = srv
            .run_task(&task_ask(&base_ask()), "peer-x", "127.0.0.1:0")
            .await;
        let v = verdict(&task).to_string();
        assert!(!v.contains("PRINCIPAL=[]"), "{v}");
        assert!(v.contains("peer:peer-x/local:owner"), "{v}");

        // Garbage is refused, and no child runs for it.
        let runs_before = std::fs::read_to_string(&ran)
            .unwrap_or_default()
            .lines()
            .count();
        let mut bad = base_ask();
        bad.principal = Some("{not a principal".into());
        let task = srv.run_task(&task_ask(&bad), "peer-x", "127.0.0.1:0").await;
        std::env::remove_var("VELES_BINARY");
        let v = verdict(&task);
        assert_eq!(v["status"], "refused", "{v}");
        assert!(v.to_string().contains("does not parse"), "{v}");
        let runs_after = std::fs::read_to_string(&ran)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(
            runs_before, runs_after,
            "no child may run for an unparseable principal"
        );
    }

    /// A re-homed tool call (D1/D2): refused from a peer not in
    /// task_peers; a host read outside the secrets runs unasked and
    /// answers with the file; what only an admin may run comes back
    /// asking for a ticket — and runs nothing.
    #[tokio::test]
    async fn a_remote_tool_call_runs_at_the_user_level_or_asks_for_a_ticket() {
        let (home, srv, _env) = scratch_server();
        let tool_ask = |tool: &str, args: Value| {
            let ask = veles_core::remotetool::RemoteToolAsk {
                from_node: "aaaa-1111".into(),
                session: "s1".into(),
                tool: tool.into(),
                args,
                principal: None,
                ticket: None,
                wire_task_id: None,
                others: false,
            };
            json!({
                "message": {"role": "user", "parts": [{"kind": "text",
                    "text": veles_core::remotetool::encode_ask(&ask).unwrap()}]}
            })
        };
        let reply = |task: &Value| {
            veles_core::remotetool::decode_reply(&veles_core::a2a::extract_text(task)).unwrap()
        };
        let f = home.path().join("from-b.txt");
        std::fs::write(&f, "the file on b").unwrap();
        let read = tool_ask("read_file", json!({"path": f.display().to_string()}));

        let task = srv.run_task(&read, "peer-x", "127.0.0.1:0").await;
        let v = reply(&task);
        assert_eq!(v["status"], "refused", "{v}");
        assert!(v["error"].as_str().unwrap().contains("task_peers"), "{v}");

        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let task = srv.run_task(&read, "peer-x", "127.0.0.1:0").await;
        let v = reply(&task);
        assert_eq!(v["status"], "ok", "{v}");
        assert!(
            v["output"].as_str().unwrap().contains("the file on b"),
            "{v}"
        );

        let task = srv
            .run_task(
                &tool_ask(
                    "cronjob",
                    json!({"action": "create", "prompt": "x", "schedule": "1h"}),
                ),
                "peer-x",
                "127.0.0.1:0",
            )
            .await;
        let v = reply(&task);
        assert_eq!(v["status"], "needs_ticket", "{v}");
        assert!(srv.store.lock_ok().cron_list(false).unwrap().is_empty());
    }

    /// A corrupted ask (right marker, bad checksum) is refused, not
    /// half-run: the fence is the integrity gate for cluster traffic.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_remote_task_with_a_bad_checksum_is_refused() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let enc = veles_core::remotetask::encode_ask(&base_ask()).unwrap();
        // Re-fence the same payload under a DIFFERENT sha.
        let tampered = enc.replace("sha256:", "sha256:ff");
        let params = json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": tampered}]}
        });
        let task = srv.run_task(&params, "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{task}"
        );
        assert!(srv.sessions.lock_ok().is_empty());
    }
}

#[cfg(test)]
mod c3_pins {
    #[cfg(unix)]
    use super::b3a_pins::{base_ask, task_ask};
    use super::tests::scratch_server;
    use super::*;

    /// A fake child that BLOCKS until cancelled: records its start, then
    /// sleeps long enough that only a kill ends it.
    #[cfg(unix)]
    fn blocking_child(home: &std::path::Path) -> std::path::PathBuf {
        let started = home.join("child-started");
        let fake = home.join("fake-veles-c3");
        std::fs::write(
            &fake,
            format!(
                // `exec`: the sleep REPLACES the shell, so the kill
                // lands on the process holding the stdout pipe. A
                // shell that spawns sleep would die on kill but
                // orphan the sleeper holding the pipe open — the
                // reader would block to its fuse and the pin would
                // measure the fake's shape, not the cancel path.
                "#!/bin/sh\ntouch {}\nexec sleep 30\n",
                started.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("VELES_BINARY", &fake);
        started
    }

    /// C3, the receiver half: an ask with a sender-minted wire_task_id
    /// registers a WORKING task under THAT id BEFORE the child finishes,
    /// and tasks/cancel on it (a) flips the token threaded into the
    /// child's kill loop — the child dies, it does not ride out its
    /// sleep — and (b) leaves the record CANCELED. Before this round the
    /// record existed only after the run, so cancel answered "not
    /// found" for the child's whole life.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_wire_task_registers_working_and_cancel_kills_the_child() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let started = blocking_child(home.path());
        let mut ask = base_ask();
        ask.goal = "c3-blocking-goal".into();
        ask.wire_task_id = Some("wire-id-c3".into());

        let params = task_ask(&ask);
        let srv2 = srv.clone();
        let run = veles_core::rt::spawn(async move {
            let task = srv2.run_task(&params, "peer-x", "127.0.0.1:0").await;
            std::env::remove_var("VELES_BINARY");
            task
        });

        // The WORKING record under the sender's id exists while the
        // child still runs — poll until the child has really started.
        let mut working = None;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if started.exists() {
                working = srv.tasks.get("wire-id-c3");
                if working.is_some() {
                    break;
                }
            }
        }
        let rec = working.expect("the WORKING record exists under the wire id, mid-run");
        assert_eq!(rec.state, STATE_WORKING, "{rec:?}");
        assert_eq!(rec.peer, "peer-x");

        // Cancel as the owning peer, through the real method table.
        let (status, body) = dispatch_method(
            srv.clone(),
            "peer-x",
            "127.0.0.1:0",
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tasks/cancel",
                "params": {"id": "wire-id-c3"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["result"]["task"]["status"]["state"].as_str(),
            Some(STATE_CANCELED),
            "{:?}",
            body.0
        );

        // The run ends PROMPTLY — the token reached the child's kill
        // loop, so the sleep 30 did not ride out.
        let task = tokio::time::timeout(std::time::Duration::from_secs(15), run)
            .await
            .expect("the child must die on cancel, not sleep 30s")
            .unwrap();
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_CANCELED),
            "the terminal record stays CANCELED: {task}"
        );
        // The record still exists (update-in-place, not a second row).
        let after = srv.tasks.get("wire-id-c3").expect("record survives");
        assert_eq!(after.state, STATE_CANCELED, "{after:?}");
        assert_eq!(
            srv.tasks
                .list(50)
                .iter()
                .filter(|r| r.id == "wire-id-c3")
                .count(),
            1,
            "one record, not a WORKING row plus a terminal row"
        );
    }

    /// C3, the gate: tasks/cancel is OWNERSHIP-SCOPED. A different
    /// authenticated peer asking for the same id gets "task not found" —
    /// existence of another peer's task is not the asker's business,
    /// and remote task ids are sender-minted (known outside this node
    /// by construction).
    #[tokio::test]
    async fn cancel_from_a_foreign_peer_is_not_found() {
        let (_home, srv, _env) = scratch_server();
        srv.tasks.put(TaskRec {
            id: "wire-owned-by-x".into(),
            context_id: "ctx".into(),
            state: STATE_WORKING.into(),
            peer: "peer-x".into(),
            inbound_text: String::new(),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        let (status, body) = dispatch_method(
            srv.clone(),
            "peer-other",
            "127.0.0.1:0",
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tasks/cancel",
                "params": {"id": "wire-owned-by-x"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["error"]["code"].as_i64(),
            Some(ERR_TASK_NOT_FOUND as i64),
            "a foreign peer cannot even learn the task exists: {:?}",
            body.0
        );
        // The owner still can, and the record was untouched by the
        // foreign attempt.
        let rec = srv.tasks.get("wire-owned-by-x").unwrap();
        assert_eq!(rec.state, STATE_WORKING, "{rec:?}");
        assert!(!rec.cancel.is_cancelled());
    }

    /// C3, the sibling gates: tasks/get and tasks/list are peer-scoped
    /// the same way cancel is — a foreign peer gets "not found" on get
    /// and never sees the other's rows in list.
    #[tokio::test]
    async fn tasks_get_and_list_are_peer_scoped() {
        let (_home, srv, _env) = scratch_server();
        srv.tasks.put(TaskRec {
            id: "owned-by-x".into(),
            context_id: "ctx".into(),
            state: STATE_WORKING.into(),
            peer: "peer-x".into(),
            inbound_text: String::new(),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        srv.tasks.put(TaskRec {
            id: "owned-by-y".into(),
            context_id: "ctx2".into(),
            state: STATE_COMPLETED.into(),
            peer: "peer-y".into(),
            inbound_text: String::new(),
            reply_text: String::new(),
            created: veles_core::a2a::now_iso(),
            cancel: Default::default(),
        });
        // x asks for y's task by id: not found.
        let (_, body) = dispatch_method(
            srv.clone(),
            "peer-x",
            "127.0.0.1:0",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get",
                    "params": {"id": "owned-by-y"}}),
        )
        .await;
        assert_eq!(
            body.0["error"]["code"].as_i64(),
            Some(ERR_TASK_NOT_FOUND as i64),
            "{:?}",
            body.0
        );
        // x asks for its own: found.
        let (_, body) = dispatch_method(
            srv.clone(),
            "peer-x",
            "127.0.0.1:0",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get",
                    "params": {"id": "owned-by-x"}}),
        )
        .await;
        assert_eq!(
            body.0["result"]["task"]["id"].as_str(),
            Some("owned-by-x"),
            "{:?}",
            body.0
        );
        // list shows each peer only its own.
        let (_, body) = dispatch_method(
            srv.clone(),
            "peer-x",
            "127.0.0.1:0",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/list", "params": {}}),
        )
        .await;
        let ids: Vec<&str> = body.0["result"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["owned-by-x"], "{:?}", body.0);
    }

    /// The B4 shape survives: an ask WITHOUT a wire_task_id (older
    /// sender) runs exactly as before — one record, put at the end,
    /// under OUR task id; no mid-run WORKING row to cancel.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_ask_without_a_wire_id_keeps_the_b4_shape() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let log = home.path().join("argv.log");
        let fake = home.path().join("fake-veles");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> {}\necho ran-on-b\n",
                log.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("VELES_BINARY", &fake);
        let mut ask = base_ask();
        ask.wire_task_id = None;
        let task = srv.run_task(&task_ask(&ask), "peer-x", "127.0.0.1:0").await;
        std::env::remove_var("VELES_BINARY");
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        let text = veles_core::a2a::extract_text(&task);
        let v = veles_core::remotetask::decode_reply(&text).unwrap();
        assert_eq!(v["status"], "ok", "{v}");
    }
}

#[cfg(test)]
mod b6_pins {
    use super::tests::scratch_server;
    use super::*;

    fn bundle_msg(to_node: &str, session: &str) -> String {
        let b = veles_core::migration::MigrationBundle {
            bundle_id: "b-1".into(),
            from_node: "node-a".into(),
            from_name: "a".into(),
            to_node: to_node.into(),
            created_at: veles_core::a2a::now_iso(),
            sessions: vec![veles_core::migration::SessionRow {
                id: session.into(),
                title: Some("t".into()),
                parent: None,
                created_at: None,
                updated_at: None,
                node: Some("node-a".into()),
                depth: 0,
            }],
            messages: vec![],
            checkpoints: vec![],
            queue: vec![],
            overrides: vec![],
            locale: vec![],
            todos: vec![],
            notes: vec![],
            prompts: vec![],
        };
        veles_core::migration::encode_bundle(&b).unwrap()
    }

    fn params(text: &str) -> Value {
        json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": text}]}
        })
    }

    /// Question 5: the migration allowlist is its OWN list — a peer
    /// that is a memsync/gossip/task peer is NOT automatically a
    /// migration peer. An unlisted peer's bundle is refused by name
    /// and NOTHING lands (no session row appears on this node).
    #[tokio::test]
    async fn a_migration_bundle_from_an_unlisted_peer_is_refused_by_name() {
        let (home, srv, _env) = scratch_server();
        // peer-x is a TASK peer but NOT a migration peer.
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        let raw = bundle_msg("this-node", "mig-refused");
        let task = srv.run_task(&params(&raw), "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{task}"
        );
        let text = veles_core::a2a::extract_text(&task);
        assert!(
            text.contains("migrate_peers"),
            "the refusal names the list it is missing from: {text}"
        );
        // Nothing landed.
        let store = srv.store.lock_ok();
        assert!(
            !store.session_exists("mig-refused").unwrap(),
            "a refused bundle creates no session"
        );
        assert!(
            srv.sessions.lock_ok().is_empty(),
            "and it is not a turn — no agent session either"
        );
    }

    /// The same allowlist, spelled as a task_peers-style contrast: an
    /// ALLOWLISTED peer's bundle to THIS node is adopted, rows land,
    /// and the reply carries the `adopted ` prefix the sender parses.
    #[tokio::test]
    async fn an_allowlisted_bundles_adopts_and_answers_with_the_adopted_prefix() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nmigrate_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        // This node needs an identity for the addressee check to pass.
        std::fs::write(
            veles_core::config::config_dir().join("node.toml"),
            "id = \"this-node\"\nname = \"me\"\n",
        )
        .unwrap();
        let raw = bundle_msg("this-node", "mig-lands");
        let task = srv.run_task(&params(&raw), "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        let text = veles_core::a2a::extract_text(&task);
        assert!(
            text.starts_with("adopted mig-lands"),
            "the reply shape the sender's migrate() parses: {text}"
        );
        let store = srv.store.lock_ok();
        assert!(store.session_exists("mig-lands").unwrap(), "the row landed");
        assert_eq!(
            store.session_node_stamp("mig-lands").unwrap().as_deref(),
            Some("this-node"),
            "stamped with the adopter"
        );
    }

    /// A bundle addressed to a DIFFERENT node is refused even from an
    /// allowlisted peer — the addressee check (question: adopt verifies
    /// to_node names itself).
    #[tokio::test]
    async fn a_bundle_addressed_to_another_node_is_refused() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nmigrate_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        std::fs::write(
            veles_core::config::config_dir().join("node.toml"),
            "id = \"this-node\"\nname = \"me\"\n",
        )
        .unwrap();
        // Addressed to "somewhere-else", not this-node.
        let raw = bundle_msg("somewhere-else", "mig-misaddressed");
        let task = srv.run_task(&params(&raw), "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{task}"
        );
        let text = veles_core::a2a::extract_text(&task);
        assert!(
            text.contains("addressed to node 'somewhere-else'"),
            "{text}"
        );
        assert!(
            !srv.store
                .lock_ok()
                .session_exists("mig-misaddressed")
                .unwrap(),
            "a misaddressed bundle lands nothing"
        );
    }

    /// The split-brain query rides the same allowlist, and answers
    /// from the store: not-adopted for a session this node lacks,
    /// adopted once it holds one live.
    #[tokio::test]
    async fn a_migration_query_is_gated_and_answers_from_the_store() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nmigrate_peers = [\"peer-x\"]\n",
        )
        .unwrap();
        // An unlisted peer is refused.
        let q = veles_core::migration::encode_query("s-1", "node-a");
        let task = srv.run_task(&params(&q), "stranger", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_REJECTED),
            "{task}"
        );
        assert!(
            veles_core::a2a::extract_text(&task).contains("migrate_peers"),
            "{task}"
        );
        // The allowlisted peer gets a store-read answer.
        let task = srv.run_task(&params(&q), "peer-x", "127.0.0.1:0").await;
        assert_eq!(
            task["status"]["state"].as_str(),
            Some(STATE_COMPLETED),
            "{task}"
        );
        assert_eq!(
            veles_core::a2a::extract_text(&task),
            "not-adopted s-1",
            "unknown here = not-adopted"
        );
        // Seed a live row and the answer flips.
        {
            let store = srv.store.lock_ok();
            store.create_session("s-1").unwrap();
        }
        let task = srv.run_task(&params(&q), "peer-x", "127.0.0.1:0").await;
        let ans = veles_core::a2a::extract_text(&task);
        assert!(ans.starts_with("adopted s-1 "), "{ans}");
    }
}

/// The 2026-09-29 review of the B5/B6 receivers: whatever a peer sends is
/// data, and the parts of it that pick files or rows are checked against
/// who actually sent it.
#[cfg(test)]
mod review_pins {
    use super::tests::scratch_server;
    use super::*;

    fn params(text: &str) -> Value {
        json!({
            "message": {"role": "user", "parts": [{"kind": "text", "text": text}]}
        })
    }

    fn store_offer(from: &str) -> String {
        let offer = veles_core::storesync::StoreOffer {
            from_node: from.into(),
            from_name: from.into(),
            since_seq: 0,
            rows: vec![],
            part: 0,
            parts: 0,
            base: 0,
        };
        veles_core::storesync::encode_message(&offer).unwrap()
    }

    /// An offer's `from_node` names the replica FILE it writes. It used
    /// to be taken as-is: `../state` opened (and stamped) the node's own
    /// state.db while the offer was merely being stored, and any listed
    /// peer could write any other node's replica.
    #[tokio::test]
    async fn a_store_offer_cannot_name_a_path_or_another_peers_node() {
        let (home, srv, _env) = scratch_server();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\nstoresync_peers = [\"peer-x\", \"peer-y\"]\n",
        )
        .unwrap();
        let t = srv
            .run_task(&params(&store_offer("../state")), "peer-x", "127.0.0.1:0")
            .await;
        assert_eq!(t["status"]["state"].as_str(), Some(STATE_REJECTED), "{t}");
        let primary = home.path().join("state.db");
        if primary.exists() {
            let s = Store::open(&primary).unwrap();
            assert!(
                !veles_core::storesync::is_replica(s.conn()),
                "the home's state.db was stamped as a replica"
            );
        }
        // First use binds peer-x to node-a …
        let t = srv
            .run_task(&params(&store_offer("node-a")), "peer-x", "127.0.0.1:0")
            .await;
        assert_eq!(t["status"]["state"].as_str(), Some(STATE_COMPLETED), "{t}");
        // … so peer-y cannot write node-a's replica.
        let t = srv
            .run_task(&params(&store_offer("node-a")), "peer-y", "127.0.0.1:0")
            .await;
        assert_eq!(t["status"]["state"].as_str(), Some(STATE_REJECTED), "{t}");
        let text = veles_core::a2a::extract_text(&t);
        assert!(
            text.contains("peer-x"),
            "names the peer that holds it: {text}"
        );
    }
}
