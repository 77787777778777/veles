//! The owner's ask relay, across nodes (2026-10-02).
//!
//! `[notify] ask_relay` follows a question nobody answered to the
//! owner's Telegram home chat ([`crate::ask`], "the cross-session
//! half"). It posted through THIS node's Telegram adapter — and in a
//! pool the adapter runs on whichever node holds the `singleton:telegram`
//! lease. Once that is the VPS, a question born on the laptop's desktop
//! was relayed to an adapter that is not there, and reached nobody.
//!
//! Now the relay leg ([`OwnerRelayAsker`]) asks here when the adapter is
//! here, and otherwise asks the node that holds the lease to put the
//! question in ITS home chat:
//!
//! ```text
//!   asking node                              lease holder
//!   OwnerRelayAsker::ask ── ask relay ──────▶ serve(): its own home
//!     (a leg of the ask here)                 chat, the owner answers
//!   resolve(id)  ◀───────────── the answer ──
//!   answered here first ──────── cancel ────▶ the question is closed
//! ```
//!
//! A question that names its round ([`AskRequest::relay_key`]: a sync
//! decision) outlives the asking node's request. A phone asks, then stops
//! for idleness five minutes later, and its request ends with it; the
//! owner's tap on Telegram then found no question here and was told "not
//! yours to answer" (2026-10-13). Now the question stays open, the answer
//! is kept, and the node's next ask with the same key — when it runs
//! again — takes it; while the question is open, that ask waits on it
//! instead of putting it to the owner twice.
//!
//! What the holder will do for a peer is narrow: put a question in its
//! OWN configured home chat — never a chat the sender names — for a peer
//! in its `[cluster] task_peers`, and send back what the owner chose.
//! A secret is never relayed (the same rule as at home). It acts on
//! nothing: the answer is only used on the node that asked.

use crate::ask::{AskAnswer, AskKind, AskRequest, Asker};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The marker of an ask relay message (the gossip family's framing).
pub const ASK_RELAY_MARKER: &str = "[veles cluster ask relay]";
/// The marker of the holder's answer.
pub const ASK_RELAY_ANSWER_MARKER: &str = "[veles cluster ask relay answer]";
const FENCE: &str = "```veles-askrelay-b64";

/// One relay message: an ask to put to the owner, or the cancel of one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRelayWire {
    /// `ask` or `cancel`.
    pub op: String,
    /// The asking node's id, and its name for the owner to read.
    pub from_node: String,
    #[serde(default)]
    pub from_name: String,
    /// The ask's id on the asking node.
    pub id: String,
    /// The question (`ask` only).
    #[serde(default)]
    pub ask: Option<AskRequest>,
    /// The question's round (`cancel` only; an `ask` carries it in the
    /// request): a cancel from a later ask of the round closes the
    /// question the first one opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Does this text carry an ask relay?
pub fn is_ask_relay(text: &str) -> bool {
    text.trim_start().starts_with(ASK_RELAY_MARKER)
}

fn frame(marker: &str, json: &str) -> String {
    let sha = crate::memory::MemoryStore::content_sha(json);
    format!(
        "{marker}\n{FENCE}\nsha256:{sha}\n{}",
        crate::gossip::b64_encode(json)
    )
}

fn unframe(raw: &str) -> Result<String> {
    let (sha, b64) = crate::gossip::fenced_payload(raw, FENCE)?;
    let json = String::from_utf8(crate::gossip::b64_decode(&b64)?)?;
    crate::gossip::check_sha(&json, &sha)?;
    Ok(json)
}

pub fn encode(w: &AskRelayWire) -> String {
    frame(
        ASK_RELAY_MARKER,
        &serde_json::to_string(w).unwrap_or_default(),
    )
}

pub fn decode(raw: &str) -> Result<AskRelayWire> {
    serde_json::from_str(&unframe(raw)?).context("payload is not an ask relay")
}

/// The holder's answer, base64 inside the frame: an owner's free text
/// must not meet the A2A redactor on its way back.
pub fn encode_answer(a: &AskAnswer) -> String {
    frame(
        ASK_RELAY_ANSWER_MARKER,
        &serde_json::to_string(a).unwrap_or_default(),
    )
}

pub fn decode_answer(raw: &str) -> Result<AskAnswer> {
    serde_json::from_str(&unframe(raw)?).context("payload is not an ask relay answer")
}

// ── the holder's side ────────────────────────────────────────────────

fn sink_cell() -> &'static crate::owned::OwnedSlot<Arc<dyn Asker>> {
    static S: OnceLock<crate::owned::OwnedSlot<Arc<dyn Asker>>> = OnceLock::new();
    S.get_or_init(crate::owned::OwnedSlot::default)
}

/// Where a relayed question is put on this node: the owner's home chat
/// here, installed by the gateway beside its own relay leg. `None`: this
/// node takes no relays. The owner is read from the calling thread (M4).
pub fn set_sink(a: Option<Arc<dyn Asker>>) {
    match a {
        Some(asker) => sink_cell().set(asker),
        None => sink_cell().clear(),
    }
}

/// Uninstall the relay sink ONLY if `owner` installed it (R3 teardown).
/// `None` (core) never matches. Returns whether it cleared.
pub fn purge_owner_sink(owner: Option<&'static str>) -> bool {
    sink_cell().purge(owner)
}

fn sink() -> Option<Arc<dyn Asker>> {
    sink_cell().with(|a| a.clone())
}

/// The id a relayed ask has HERE. Ask ids are made per process, so two
/// nodes can mint the same one; a relayed id is kept apart from this
/// node's own asks by the asking node's id.
fn local_id(from_node: &str, id: &str) -> String {
    let short: String = from_node.chars().take(8).collect();
    format!("relay:{short}:{id}")
}

/// Serve one relay message from a peer (the A2A server has checked that
/// the peer is in `[cluster] task_peers`). `ask`: the owner's answer, or
/// a timeout when nobody answered. `cancel`: closes the question here.
pub async fn serve(wire: AskRelayWire) -> Result<AskAnswer, String> {
    let id = local_id(&wire.from_node, &wire.id);
    match wire.op.as_str() {
        "cancel" => {
            crate::ask::cancel(&id);
            if let Some(key) = &wire.key {
                let round = round_id(&wire.from_node, key);
                let mut r = rounds().lock().unwrap_or_else(|p| p.into_inner());
                r.answered.remove(&round);
                if let Some((question, _)) = r.open.get(&round) {
                    crate::ask::cancel(question);
                }
            }
            Ok(AskAnswer::timeout())
        }
        "ask" => {
            let Some(mut req) = wire.ask else {
                return Err("an ask relay without the ask".into());
            };
            if req.kind == AskKind::Secret {
                return Err("a secret is never relayed".into());
            }
            let Some(sink) = sink() else {
                return Err("this node puts no relayed questions anywhere: it has no \
                            [platforms.telegram.home_channel], or [notify] ask_relay is off"
                    .into());
            };
            let from = if wire.from_name.trim().is_empty() {
                wire.from_node.clone()
            } else {
                wire.from_name.clone()
            };
            req.id = id;
            req.prompt = format!(
                "({}) {}",
                crate::i18n!("from node %{node}", node = from),
                req.prompt
            );
            match req.relay_key.clone() {
                Some(key) => Ok(put_round(sink, round_id(&wire.from_node, &key), req, &from).await),
                None => {
                    tracing::info!("ask relay: {} from node {from}, put to the owner", req.id);
                    Ok(sink.ask(req).await)
                }
            }
        }
        other => Err(format!("unknown ask relay op `{other}`")),
    }
}

/// The rounds peers named, on this node: the question open for each (its
/// id here, and the answer once given), and the answers the owner gave.
#[derive(Default)]
struct Rounds {
    open: HashMap<String, (String, tokio::sync::watch::Receiver<Option<AskAnswer>>)>,
    answered: HashMap<String, (AskAnswer, u64)>,
}

/// How long an answer waits for its node to come back for it.
const KEEP_ANSWER_SECS: u64 = 7 * 24 * 3600;

fn rounds() -> &'static Mutex<Rounds> {
    static R: OnceLock<Mutex<Rounds>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// A round's name here: its key, kept apart per asking node.
fn round_id(from_node: &str, key: &str) -> String {
    let short: String = from_node.chars().take(8).collect();
    format!("{short}:{key}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Put one ask of a round to the owner: the answer if it was given
/// already, the open question's answer if it is still out, else a new
/// question — asked on a task of its own, so it stays open when the
/// asking node's request ends. Each question is registered under the id
/// of the ask that opened it: an id this node closed is never reused
/// (`ask::register` refuses a closed id).
async fn put_round(sink: Arc<dyn Asker>, round: String, req: AskRequest, from: &str) -> AskAnswer {
    let mut rx = {
        let mut r = rounds().lock().unwrap_or_else(|p| p.into_inner());
        let now = now_secs();
        r.answered
            .retain(|_, (_, at)| now.saturating_sub(*at) < KEEP_ANSWER_SECS);
        if let Some((a, _)) = r.answered.get(&round) {
            tracing::info!("ask relay: round {round} from node {from} was answered — handed over");
            return a.clone();
        }
        if let Some((question, rx)) = r.open.get(&round) {
            tracing::info!(
                "ask relay: round {round} from node {from} is still open here ({question}) — \
                 waiting on it, not asking again"
            );
            rx.clone()
        } else {
            let (tx, rx) = tokio::sync::watch::channel(None);
            r.open.insert(round.clone(), (req.id.clone(), rx.clone()));
            tracing::info!(
                "ask relay: {} from node {from}, put to the owner (round {round})",
                req.id
            );
            crate::rt::spawn(async move {
                let a = sink.ask(req).await;
                {
                    let mut r = rounds().lock().unwrap_or_else(|p| p.into_inner());
                    r.open.remove(&round);
                    if !a.timed_out {
                        r.answered.insert(round, (a.clone(), now_secs()));
                    }
                }
                let _ = tx.send(Some(a));
            });
            rx
        }
    };
    loop {
        if let Some(a) = rx.borrow_and_update().clone() {
            return a;
        }
        if rx.changed().await.is_err() {
            return AskAnswer::timeout();
        }
    }
}

// ── the asking node's side ───────────────────────────────────────────

/// The owner's off-surface leg ([`crate::ask::set_owner_fallback`]):
/// asks through this node's adapter when it runs here, and otherwise
/// through the node that holds the platform's lease.
pub struct OwnerRelayAsker {
    /// The home chat on this node's bus.
    pub local: Arc<dyn Asker>,
    pub bus: crate::bus::GatewayBus,
    pub platform: String,
    pub store: Arc<Mutex<crate::store::Store>>,
}

#[async_trait]
impl Asker for OwnerRelayAsker {
    async fn ask(&self, req: AskRequest) -> AskAnswer {
        // Decided per ask: the adapter comes and goes with its lease.
        if self.bus.has_adapter(&self.platform) {
            return self.local.ask(req).await;
        }
        relay_to_holder(&self.store, &self.platform, req).await
    }
}

/// Put `req` to the owner through the node that holds
/// `singleton:<platform>`, as one leg of the ask here.
async fn relay_to_holder(
    store: &Arc<Mutex<crate::store::Store>>,
    platform: &str,
    req: AskRequest,
) -> AskAnswer {
    let fail = |why: String| {
        tracing::warn!("ask {}: not relayed — {why}", req.id);
        AskAnswer::timeout()
    };
    let cfg = match crate::config::load() {
        Ok(c) => Arc::new(c),
        Err(e) => return fail(format!("config: {e:#}")),
    };
    let arbiter = match crate::arbiter::StaticArbiter::new(store.clone(), cfg.clone()) {
        Ok(a) => a,
        Err(e) => {
            return fail(format!(
                "no {platform} adapter runs here, and there is no arbiter to say which node \
                 runs it: {e:#}"
            ))
        }
    };
    use crate::arbiter::{Arbiter as _, LeaseState};
    let holder = match arbiter.lease_status(&format!("singleton:{platform}")).await {
        LeaseState::HeldByOther { holder, .. } => holder,
        LeaseState::Held(_) => {
            return fail(format!(
                "this node holds the {platform} lease but runs no {platform} adapter"
            ))
        }
        LeaseState::Refused(why) => return fail(format!("no node runs {platform}: {why}")),
    };
    let row = {
        let s = store.lock().unwrap_or_else(|p| p.into_inner());
        s.node_get(&holder).ok().flatten()
    };
    let Some(row) = row else {
        return fail(format!(
            "node {holder} holds {platform} but is not in the registry"
        ));
    };
    let Some((url, bearer)) = crate::delegate::remote_target_for(&row) else {
        return fail(format!(
            "node {} runs {platform}, and it has no [a2a_agents] entry here",
            row.name
        ));
    };
    let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten();
    let from_node = me.as_ref().map(|i| i.id.clone()).unwrap_or_default();
    let from_name = cfg
        .node
        .name
        .clone()
        .or_else(|| me.map(|i| i.name))
        .unwrap_or_default();
    // A leg here, so an answer anywhere else ends this one — and the
    // question there.
    let Some(leg) = crate::ask::register(&req, None, None) else {
        return AskAnswer::timeout();
    };
    let ask = AskRelayWire {
        op: "ask".into(),
        from_node: from_node.clone(),
        from_name: from_name.clone(),
        id: req.id.clone(),
        ask: Some(req.clone()),
        key: None,
    };
    let client = crate::a2a::A2AClient::with_timeout(std::time::Duration::from_secs(
        req.timeout_secs.saturating_add(30),
    ));
    let ctx = crate::a2a::new_context_id();
    tracing::info!(
        "ask {}: relayed to node {} for its {platform} home chat",
        req.id,
        row.name
    );
    let ask_text = encode(&ask);
    let cancel_text = encode(&AskRelayWire {
        op: "cancel".into(),
        from_node,
        from_name,
        id: req.id.clone(),
        ask: None,
        key: req.relay_key.clone(),
    });
    let (cancel_url, cancel_bearer) = (url.clone(), bearer.clone());
    tokio::select! {
        answered = leg.wait() => {
            crate::rt::spawn(async move {
                let _ = crate::a2a::A2AClient::new()
                    .call(
                        &cancel_url,
                        cancel_bearer.as_deref(),
                        &cancel_text,
                        &crate::a2a::new_context_id(),
                    )
                    .await;
            });
            answered
        }
        r = client.call(&url, bearer.as_deref(), &ask_text, &ctx) => {
            let got = match r {
                Ok(v) => decode_answer(&crate::a2a::extract_text(&v)).map_err(|e| {
                    format!("{e:#} — node {} said: {}", row.name, crate::a2a::summarize_result(&v))
                }),
                Err(e) => Err(format!("node {}: {e:#}", row.name)),
            };
            match got {
                Ok(a) if !a.timed_out => {
                    crate::ask::resolve(&req.id, a.clone());
                    a
                }
                Ok(a) => a,
                Err(why) => fail(why),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: AskKind) -> AskRequest {
        AskRequest::new("s1", kind, "run `ls`?", crate::ask::approval_choices(true))
    }

    /// The frames survive the trip and refuse a payload that changed in
    /// transit; an owner's free text rides as base64, untouched.
    #[test]
    fn the_relay_frames_round_trip_and_refuse_tampering() {
        let w = AskRelayWire {
            op: "ask".into(),
            from_node: "38a5e4dd-x".into(),
            from_name: "laptop".into(),
            id: "ask3-1f".into(),
            ask: Some(req(AskKind::Approval)),
            key: None,
        };
        let enc = encode(&w);
        assert!(is_ask_relay(&enc));
        let back = decode(&enc).unwrap();
        assert_eq!(back.id, "ask3-1f");
        assert_eq!(back.ask.unwrap().prompt, "run `ls`?");
        let bad = enc.replace("sha256:", "sha256:0");
        assert!(decode(&bad).is_err());

        let a = AskAnswer {
            free_text: Some("mail me at someone@example.org".into()),
            ..AskAnswer::choice("approve")
        };
        let enc = encode_answer(&a);
        assert!(!is_ask_relay(&enc), "an answer is not a relay");
        let back = decode_answer(&enc).unwrap();
        assert_eq!(
            back.free_text.as_deref(),
            Some("mail me at someone@example.org")
        );
        assert_eq!(back.selected, vec!["approve".to_string()]);
    }

    /// What the holder refuses: a secret, a relay with no sink here, a
    /// relay without its ask, an unknown op.
    #[tokio::test]
    async fn the_holder_refuses_what_it_must() {
        // The sink is process-wide: serialize with the test that sets it.
        let _g = crate::testenv::guard();
        set_sink(None);
        let mk = |op: &str, ask: Option<AskRequest>| AskRelayWire {
            op: op.into(),
            from_node: "n1".into(),
            from_name: "laptop".into(),
            id: crate::testenv::chat_key(),
            ask,
            key: None,
        };
        let e = serve(mk("ask", Some(req(AskKind::Secret))))
            .await
            .unwrap_err();
        assert!(e.contains("secret"), "{e}");
        let e = serve(mk("ask", Some(req(AskKind::Approval))))
            .await
            .unwrap_err();
        assert!(e.contains("home_channel"), "{e}");
        let e = serve(mk("ask", None)).await.unwrap_err();
        assert!(e.contains("without the ask"), "{e}");
        let e = serve(mk("push", None)).await.unwrap_err();
        assert!(e.contains("unknown"), "{e}");
    }

    /// A sink that answers: records what it was asked and approves.
    struct Recorder(Mutex<Vec<AskRequest>>);
    #[async_trait]
    impl Asker for Recorder {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            self.0.lock().unwrap().push(req);
            AskAnswer::choice("approve")
        }
    }

    /// The holder puts the question in its own sink, under an id kept
    /// apart from its own asks and naming the asking node, and returns
    /// the owner's answer.
    #[tokio::test]
    async fn the_holder_asks_its_own_home_chat_and_answers() {
        let _g = crate::testenv::guard();
        let rec = Arc::new(Recorder(Mutex::new(Vec::new())));
        set_sink(Some(rec.clone()));
        let w = AskRelayWire {
            op: "ask".into(),
            from_node: "38a5e4dd-d804".into(),
            from_name: "laptop".into(),
            id: "ask7-ab".into(),
            ask: Some(req(AskKind::Approval)),
            key: None,
        };
        let a = serve(w).await.unwrap();
        set_sink(None);
        assert_eq!(a.selected, vec!["approve".to_string()]);
        let asked = rec.0.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].id, "relay:38a5e4dd:ask7-ab");
        assert!(asked[0].prompt.contains("laptop"), "{}", asked[0].prompt);
        assert!(
            asked[0].prompt.ends_with("run `ls`?"),
            "{}",
            asked[0].prompt
        );
    }

    /// A sink the test answers when it says so; it counts the questions.
    struct OnCue {
        asked: Mutex<Vec<AskRequest>>,
        go: tokio::sync::Notify,
    }
    #[async_trait]
    impl Asker for OnCue {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            self.asked.lock().unwrap().push(req);
            self.go.notified().await;
            AskAnswer::choice("apply")
        }
    }

    /// An answer within ten seconds, or the test fails: an ask that is
    /// never answered must turn this red, not hang it.
    async fn within<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(10), f)
            .await
            .expect("no answer within 10 s")
    }

    /// A round's question outlives the asking node's request
    /// (2026-10-13: a phone asked, stopped for idleness, and the owner's
    /// tap found nothing). The phone's request ends before the owner
    /// answers; the answer is kept, and the phone's next ask in the round
    /// takes it at once. While a round's question is open, another ask
    /// in it waits on that one: the owner is asked once per round.
    #[tokio::test]
    async fn a_rounds_answer_waits_for_the_node_that_asked() {
        let _g = crate::testenv::guard();
        let sink = Arc::new(OnCue {
            asked: Mutex::new(Vec::new()),
            go: tokio::sync::Notify::new(),
        });
        set_sink(Some(sink.clone()));
        let wire = |id: &str, key: &str| {
            let mut ask = req(AskKind::Approval);
            ask.relay_key = Some(key.to_string());
            AskRelayWire {
                op: "ask".into(),
                from_node: "b71e9b77-34aa".into(),
                from_name: "phone".into(),
                id: id.into(),
                ask: Some(ask),
                key: None,
            }
        };
        let asked = || sink.asked.lock().unwrap().len();
        let k1 = format!("config:a@{}", crate::testenv::chat_key());

        // Asked, and the phone stops: its request ends unanswered.
        let gone = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            serve(wire("ask1-aa", &k1)),
        )
        .await;
        assert!(gone.is_err(), "nobody has answered yet");
        assert_eq!(asked(), 1);
        // The owner answers with nobody waiting.
        sink.go.notify_one();
        for _ in 0..200 {
            if rounds()
                .lock()
                .unwrap()
                .answered
                .contains_key(&round_id("b71e9b77-34aa", &k1))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // The phone runs again and asks in the round: the answer, at once.
        let a = within(serve(wire("ask1-bb", &k1))).await.unwrap();
        assert_eq!(a.selected, vec!["apply".to_string()]);
        assert_eq!(asked(), 1, "not asked again");

        // Two asks in an open round: one question, both answered.
        let k2 = format!("config:b@{}", crate::testenv::chat_key());
        let first = crate::rt::spawn(serve(wire("ask2-aa", &k2)));
        for _ in 0..200 {
            if asked() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let second = crate::rt::spawn(serve(wire("ask2-bb", &k2)));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(asked(), 2, "the open question is not put twice");
        sink.go.notify_one();
        assert_eq!(
            within(first).await.unwrap().unwrap().selected,
            vec!["apply".to_string()]
        );
        assert_eq!(
            within(second).await.unwrap().unwrap().selected,
            vec!["apply".to_string()]
        );
        // Each question here has the id of the ask that opened it.
        let ids: Vec<String> = sink
            .asked
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec!["relay:b71e9b77:ask1-aa", "relay:b71e9b77:ask2-aa"]
        );
        set_sink(None);
    }
}
