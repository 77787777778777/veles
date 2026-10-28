//! Registry gossip (Plan B, milestone B3a, 2026-09-27).
//!
//! B1's registry is per-host by design — every node writes its own
//! `state.db`. Gossip is what merges the views: on the heartbeat tick
//! (the `[cluster] gossip_interval_secs` cadence B1 already runs), a
//! node sends its registry rows to each configured gossip peer over the
//! authenticated A2A link, the receiver merges them into its own
//! registry ([`Store::node_merge_from_gossip`]), and answers with ITS
//! rows so one exchange converges both sides. Observations never
//! travel ([`GossipNodeRow`] has no observed_* fields — a peer's view
//! of how I reach it is its evidence, not mine to overwrite).
//!
//! ## Transport: the memsync shape, minus the inbox
//!
//! Memory/config/vault offers are DATA — they land in an inbox and the
//! operator applies by hand. Gossip is different: registry rows are
//! merged ON RECEIPT, because a registry that needed hand-application
//! per exchange would never converge on a 30s cadence, and the merge
//! is idempotent and conflict-free by construction (each node authors
//! only its own row; `registered_at` is preserved; local observations
//! are never touched). Security is the A2A layer's: only peers named
//! in `[cluster] memsync_peers` may exchange gossip (same allowlist,
//! same reasoning — one list of "peers allowed to talk cluster
//! business with me"), and the message never reaches an agent turn.
//!
//! ## What rides with it
//!
//! The same marker family carries the singleton lease ask/answer
//! ([`arbiter`]) and the peer-observed address ECHO — every gossip
//! ANSWER tells the asker "you appear as <addr>", which the asker
//! records against its own node id as observed by that peer. That
//! echo is how the peer-observed static-IP evidence (PLAN "Gap in B1")
//! accumulates without a second protocol: the sequence of addresses a
//! node's peers saw it arrive from, over days, is the evidence for
//! whether its uplink is static.

use crate::store::GossipNodeRow;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const GOSSIP_MARKER: &str = "[veles cluster gossip]";
const FENCE_B64: &str = "```veles-gossip-b64";

/// Does this text carry cluster gossip?
pub fn is_gossip_message(text: &str) -> bool {
    text.trim_start().starts_with(GOSSIP_MARKER)
}

/// Does this text carry a lease ask (the arbiter protocol)?
pub fn is_lease_ask(text: &str) -> bool {
    text.trim_start()
        .starts_with(crate::arbiter::LEASE_ASK_MARKER)
}

/// One gossip exchange over the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipMessage {
    /// The sender's node id — who is asking. The receiver records its
    /// reflexive observation of the SENDER against this id, so a peer
    /// cannot make us observe a third node by listing its rows.
    pub from_node: String,
    /// The sender's registry rows (observations already dropped).
    pub rows: Vec<GossipNodeRow>,
}

/// The answer to one gossip exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipReply {
    /// The answerer's registry rows.
    pub rows: Vec<GossipNodeRow>,
    /// The address the answering node saw the asker arrive from — the
    /// ECHO that makes peer-observed evidence accumulate per exchange.
    pub you_appear_as: String,
    /// Rows in the asker's message we had never seen (for logging).
    pub merged_new: usize,
    /// Rows we already knew (for logging).
    pub merged_known: usize,
}

// ── encode/decode (the memsync marker+sha+base64 shape) ─────────────

pub(crate) fn b64_encode(json: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

pub(crate) fn b64_decode(b64: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .context("payload is not base64")
}

/// Pull the fenced payload out of a marker message: (sha, base64).
pub(crate) fn fenced_payload(raw: &str, fence: &str) -> Result<(String, String)> {
    let start = raw.find(fence).context("no payload fence")?;
    let body = &raw[start + fence.len()..];
    let body = &body[..body.find("```").unwrap_or(body.len())];
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    let sha = lines
        .next()
        .and_then(|l| l.strip_prefix("sha256:"))
        .context("missing sha256 line")?
        .to_string();
    let b64: String = lines.collect();
    Ok((sha, b64))
}

pub(crate) fn check_sha(json: &str, sha: &str) -> Result<()> {
    if crate::memory::MemoryStore::content_sha(json) != sha {
        anyhow::bail!("checksum mismatch — the payload changed in transit");
    }
    Ok(())
}

/// Encode a gossip exchange as an A2A message (marker + base64 + sha).
pub fn encode_message(msg: &GossipMessage) -> Result<String> {
    let json = serde_json::to_string(msg)?;
    let sha = crate::memory::MemoryStore::content_sha(&json);
    Ok(format!(
        "{GOSSIP_MARKER} {} rows from {}\n\
         Registry exchange over the cluster link; merged on receipt.\n\
         {FENCE_B64}\nsha256:{sha}\n{}\n```",
        msg.rows.len(),
        msg.from_node,
        b64_encode(&json)
    ))
}

/// The gossip message inside a text (fence verified against its sha).
pub fn decode_message(raw: &str) -> Result<GossipMessage> {
    let (sha, b64) = fenced_payload(raw, FENCE_B64)?;
    let json = String::from_utf8(b64_decode(&b64)?)?;
    check_sha(&json, &sha)?;
    serde_json::from_str(&json).context("payload is not a gossip exchange")
}

/// Encode a lease ask (the arbiter wire protocol, same framing).
pub fn encode_lease_ask(key: &str, holder: &str, acquire: bool) -> String {
    let ask = crate::arbiter::LeaseAskWire {
        key: key.to_string(),
        holder: holder.to_string(),
        acquire,
    };
    let json = serde_json::to_string(&ask).unwrap();
    let sha = crate::memory::MemoryStore::content_sha(&json);
    format!(
        "{}\nlease ask over the cluster link\n{FENCE_B64}\nsha256:{sha}\n{}",
        crate::arbiter::LEASE_ASK_MARKER,
        b64_encode(&json)
    )
}

/// Parse a lease ask out of a message.
pub fn decode_lease_ask(raw: &str) -> Result<crate::arbiter::LeaseAskWire> {
    let (sha, b64) = fenced_payload(raw, FENCE_B64)?;
    let json = String::from_utf8(b64_decode(&b64)?)?;
    check_sha(&json, &sha)?;
    serde_json::from_str(&json).context("payload is not a lease ask")
}

/// Encode a lease answer.
pub fn encode_lease_answer(ans: &crate::arbiter::LeaseAnswerWire) -> String {
    let json = serde_json::to_string(ans).unwrap();
    let sha = crate::memory::MemoryStore::content_sha(&json);
    format!(
        "{}\n{FENCE_B64}\nsha256:{sha}\n{}",
        crate::arbiter::LEASE_ANSWER_MARKER,
        b64_encode(&json)
    )
}

/// Parse a lease answer out of a message.
pub fn decode_lease_answer(raw: &str) -> crate::arbiter::LeaseState {
    let (sha, b64) = match fenced_payload(raw, FENCE_B64) {
        Ok(v) => v,
        Err(e) => {
            return crate::arbiter::LeaseState::Refused(format!(
                "arbiter answered, but not in a shape we can read: {e:#}"
            ))
        }
    };
    let bytes = match b64_decode(&b64) {
        Ok(b) => b,
        Err(e) => {
            return crate::arbiter::LeaseState::Refused(format!(
                "arbiter's lease answer was not base64: {e:#}"
            ))
        }
    };
    // A wrong sha on an ANSWER is also a refusal — never a guess.
    if let Ok(json) = String::from_utf8(bytes.clone()) {
        if check_sha(&json, &sha).is_err() {
            return crate::arbiter::LeaseState::Refused(
                "arbiter's lease answer failed its checksum".to_string(),
            );
        }
    }
    match serde_json::from_slice::<crate::arbiter::LeaseAnswerWire>(&bytes) {
        Ok(w) => w.into(),
        Err(e) => crate::arbiter::LeaseState::Refused(format!(
            "arbiter's lease answer was not a lease answer: {e}"
        )),
    }
}

// ── the exchange itself ─────────────────────────────────────────────

/// Handle an inbound gossip exchange: merge rows, observe the caller,
/// answer with our rows + the echo. `client_addr` is the reflexive
/// address the call arrived from (the A2A layer's ConnectInfo).
///
/// Refuses (Err) when the peer is not in `[cluster] memsync_peers` —
/// the same allowlist gate as memory/config sync.
pub fn receive_gossip(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
    peer: &str,
    client_addr: &str,
    raw: &str,
) -> Result<String> {
    if !cfg.cluster.memsync_peers.iter().any(|p| p == peer) {
        anyhow::bail!("gossip refused: '{peer}' is not in [cluster] memsync_peers");
    }
    let msg = decode_message(raw)?;
    // One write transaction for the merge, the observation and the
    // reply's read, so the exchange commits once. Each commit is an
    // fsync, and the caller holds the shared store lock throughout.
    // With a build writing to the same disk, those fsyncs took seconds,
    // and gossip (every 30 s from each peer) was the store lock's worst
    // holder: in the week to 2026-10-13 it had 2,953 holds over 100 ms,
    // 70 over 5 s, and the longest was 20 s. It made five commits.
    let tx = store.write_tx()?;
    let mut merged_new = 0usize;
    let mut merged_known = 0usize;
    for row in &msg.rows {
        let knew = store.node_get(&row.id)?.is_some();
        match store.node_merge_from_gossip(row) {
            Ok(_) => {
                if knew {
                    merged_known += 1;
                } else {
                    merged_new += 1;
                }
            }
            Err(e) => tracing::warn!("gossip: row {} failed to merge: {e:#}", row.id),
        }
    }
    // Observe the SENDER: the reflexive address this call arrived from,
    // recorded against the node id it named. The allowlist gate above
    // is what makes the claim trustworthy enough to record.
    let _ = store.node_observe(&msg.from_node, client_addr, peer);
    let rows: Vec<GossipNodeRow> = store
        .nodes_list()?
        .iter()
        .map(GossipNodeRow::from_row)
        .collect();
    tx.commit()?;
    let reply = GossipReply {
        rows,
        you_appear_as: client_addr.to_string(),
        merged_new,
        merged_known,
    };
    let json = serde_json::to_string(&reply)?;
    let sha = crate::memory::MemoryStore::content_sha(&json);
    Ok(format!(
        "{}\n{FENCE_B64}\nsha256:{sha}\n{}\n```",
        crate::arbiter::GOSSIP_REPLY_MARKER,
        b64_encode(&json)
    ))
}

/// Fold a gossip REPLY (the answer to our exchange) into our store, and
/// record the echo against the peer that reported it. Returns
/// (rows merged, the address we appear as).
pub fn receive_gossip_reply(
    store: &crate::store::Store,
    peer_name: &str,
    raw: &str,
) -> Result<(usize, String)> {
    let (sha, b64) = fenced_payload(raw, FENCE_B64)?;
    let json = String::from_utf8(b64_decode(&b64)?)?;
    check_sha(&json, &sha)?;
    let reply: GossipReply = serde_json::from_str(&json).context("not a gossip reply")?;
    // One commit for the whole fold, as in `receive_gossip`.
    let tx = store.write_tx()?;
    let mut merged = 0usize;
    for row in &reply.rows {
        if store.node_merge_from_gossip(row).is_ok() {
            merged += 1;
        }
    }
    // The echo: THIS node, as the peer sees us. Evidence about OUR
    // uplink, recorded against the peer that reported it.
    if !reply.you_appear_as.is_empty() {
        if let Ok(Some(me)) = crate::node::NodeIdentity::load(&crate::config::config_dir()) {
            let _ = store.node_observe(&me.id, &reply.you_appear_as, peer_name);
        }
    }
    tx.commit()?;
    Ok((merged, reply.you_appear_as))
}

/// One outbound gossip exchange with a peer: send ours, fold the reply.
/// Returns a one-line report for the log.
pub async fn exchange_with_peer(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    peer_name: &str,
    peer_url: &str,
    bearer: Option<&str>,
) -> Result<String> {
    // Only the registry read needs the store; the identity file and the
    // encoding come after the lock is released.
    let rows: Vec<GossipNodeRow> = crate::store::lock_timed(store, "gossip::exchange")
        .nodes_list()?
        .iter()
        .map(GossipNodeRow::from_row)
        .collect();
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?.id;
    let text = encode_message(&GossipMessage {
        from_node: me.clone(),
        rows,
    })?;
    let client = crate::a2a::A2AClient::shared();
    let ctx = crate::a2a::new_context_id();
    let result = client.call(peer_url, bearer, &text, &ctx).await?;
    let reply_text = crate::a2a::extract_text(&result);
    if !reply_text
        .trim_start()
        .starts_with(crate::arbiter::GOSSIP_REPLY_MARKER)
    {
        anyhow::bail!("{peer_name} did not answer gossip with a gossip reply");
    }
    let (merged, echo) = {
        let s = crate::store::lock_timed(store, "gossip::exchange_reply");
        receive_gossip_reply(&s, peer_name, &reply_text)?
    };
    let _ = me;
    Ok(format!(
        "{peer_name}: merged {merged} row(s); we appear as {echo}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str) -> GossipNodeRow {
        GossipNodeRow {
            id: id.to_string(),
            name: name.to_string(),
            host: "h".into(),
            version: "0".into(),
            uptime_class: "ephemeral".into(),
            gpu: None,
            endpoint: None,
            static_ip: None,
            static_ip_source: None,
            caps: None,
            load1: None,
            load_at: None,
            sessions_served: 0,
            registered_at: "2026-09-27 00:00:00".into(),
            heartbeat_at: "2026-09-27 00:00:00".into(),
        }
    }

    fn a_message() -> String {
        encode_message(&GossipMessage {
            from_node: "aaaa".into(),
            rows: vec![row("aaaa", "vps"), row("bbbb", "laptop")],
        })
        .unwrap()
    }

    /// Wire round trip: rows survive encode → decode byte-checked.
    #[test]
    fn gossip_message_round_trip() {
        let text = a_message();
        assert!(is_gossip_message(&text));
        let back = decode_message(&text).unwrap();
        assert_eq!(back.from_node, "aaaa");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0].name, "vps");
    }

    /// A tampered payload is refused by the checksum.
    #[test]
    fn a_tampered_payload_is_refused() {
        let text = a_message();
        let mut tampered = text.clone();
        let pos = tampered.find(FENCE_B64).unwrap() + FENCE_B64.len() + 20;
        tampered.replace_range(pos..pos + 1, "X");
        assert!(decode_message(&tampered).is_err());
    }

    /// The lease ask/answer wire round trip keeps the three states.
    #[test]
    fn lease_ask_answer_round_trip() {
        let ask = encode_lease_ask("singleton:cron", "node-a", true);
        assert!(is_lease_ask(&ask));
        let parsed = decode_lease_ask(&ask).unwrap();
        assert_eq!(parsed.key, "singleton:cron");
        assert_eq!(parsed.holder, "node-a");

        for (wire, expect) in [
            (
                crate::arbiter::LeaseAnswerWire {
                    state: "held".into(),
                    until: "t".into(),
                    ..Default::default()
                },
                "held",
            ),
            (
                crate::arbiter::LeaseAnswerWire {
                    state: "held_by_other".into(),
                    holder: "node-b".into(),
                    until: "t".into(),
                    ..Default::default()
                },
                "other",
            ),
            (
                crate::arbiter::LeaseAnswerWire {
                    state: "refused".into(),
                    why: "down".into(),
                    ..Default::default()
                },
                "refused",
            ),
        ] {
            let ans = encode_lease_answer(&wire);
            let state = decode_lease_answer(&ans);
            let which = match state {
                crate::arbiter::LeaseState::Held(_) => "held",
                crate::arbiter::LeaseState::HeldByOther { .. } => "other",
                crate::arbiter::LeaseState::Refused(_) => "refused",
            };
            assert_eq!(which, expect);
        }
    }

    /// An exchange converges both registries: sender rows land in the
    /// receiver, the observation is recorded against the NAMED sender,
    /// and the reply folds back. Empty allowlist refuses.
    #[test]
    fn an_exchange_converges_both_registries() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("r.db")).unwrap();

        // Empty allowlist: refused, nothing merged.
        let r = receive_gossip(
            &store,
            &crate::config::VelesConfig::default(),
            "vps",
            "10.0.30.1:52322",
            &a_message(),
        );
        assert!(r.is_err(), "empty allowlist must refuse");
        assert!(store.nodes_list().unwrap().is_empty());

        // Allowlisted: merged, observed, replied.
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.memsync_peers = vec!["vps".to_string()];
        let reply_text =
            receive_gossip(&store, &cfg, "vps", "10.0.30.1:52322", &a_message()).unwrap();
        assert!(reply_text.starts_with(crate::arbiter::GOSSIP_REPLY_MARKER));
        let rows = store.nodes_list().unwrap();
        assert_eq!(rows.len(), 2);
        let vps = rows.iter().find(|r| r.id == "aaaa").unwrap();
        assert_eq!(vps.observed_addr.as_deref(), Some("10.0.30.1:52322"));

        // The reply folds back (sender side): our own rows come home
        // as no-ops, and the echo is the address.
        let (merged, echo) = receive_gossip_reply(&store, "vps", &reply_text).unwrap();
        assert_eq!(merged, 2, "both rows merge (as known now)");
        assert_eq!(echo, "10.0.30.1:52322");
    }

    /// An exchange commits once. Each commit is an fsync, made with the
    /// shared store lock held, and five of them per exchange were
    /// gossip's multi-second holds on a busy disk (2026-10-13). Counted
    /// in WAL frames: separate commits write the `nodes` page once each.
    #[test]
    fn an_exchange_is_one_commit() {
        let fresh = |name: &str| {
            let dir = tempfile::tempdir().unwrap();
            let store = crate::store::Store::open(&dir.path().join(name)).unwrap();
            store
                .conn()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .unwrap();
            (dir, store)
        };
        let frames = |store: &crate::store::Store| -> i64 {
            store
                .conn()
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(1))
                .unwrap()
        };
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.memsync_peers = vec!["vps".to_string()];
        let (_a, one) = fresh("one.db");
        receive_gossip(&one, &cfg, "vps", "10.0.30.1:52322", &a_message()).unwrap();
        // The same writes, each committing on its own (the old shape).
        let (_b, many) = fresh("many.db");
        let msg = decode_message(&a_message()).unwrap();
        for row in &msg.rows {
            many.node_merge_from_gossip(row).unwrap();
        }
        many.node_observe(&msg.from_node, "10.0.30.1:52322", "vps")
            .unwrap();
        let (one, many) = (frames(&one), frames(&many));
        assert!(
            one < many,
            "one commit writes {one} frames, separate ones {many}"
        );
    }
}
