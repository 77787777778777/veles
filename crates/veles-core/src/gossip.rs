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

//!
//! ## Forgetting a node fleet-wide
//!
//! A forget used to delete one store's row, and the next exchange with
//! any peer that still listed the node merged it straight back. A
//! forget now leaves a tombstone (`store::NodeTombstone`: id,
//! forgotten_at), and both the message and the reply carry this node's
//! tombstones in a list of their own. The receiver folds them BEFORE the
//! rows: a tombstone drops the row unless the node has heartbeat since
//! the forget, and the merge then refuses every copy that is not newer
//! than it ([`crate::store::Store::node_merge_from_gossip`]). The list is
//! an added field, absent when empty: a peer before 0.2.0 ignores it (no
//! `deny_unknown_fields` anywhere on this wire) and keeps listing the
//! node, and one that sends no list reads as an empty one here.

use crate::store::{GossipNodeRow, NodeTombstone};
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
    /// The nodes the sender forgot fleet-wide. Absent from the wire
    /// when empty, and ignored by a peer before 0.2.0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tombstones: Vec<NodeTombstone>,
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
    /// The answerer's tombstones (see [`GossipMessage::tombstones`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tombstones: Vec<NodeTombstone>,
}

/// Fold tombstones, then rows, into `store` (inside the caller's
/// transaction). Returns (rows merged, rows a tombstone dropped). A
/// tombstone is judged by THIS node's `stale_after_secs`: it evicts only
/// a node that is stale here too (FLEET-4).
fn fold(
    store: &crate::store::Store,
    tombstones: &[NodeTombstone],
    rows: &[GossipNodeRow],
    stale_after_secs: u64,
) -> (usize, usize) {
    let mut forgotten = 0usize;
    for t in tombstones {
        match store.node_tombstone_merge_within(t, stale_after_secs as i64) {
            Ok(true) => forgotten += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!("gossip: tombstone {} failed to merge: {e:#}", t.id),
        }
    }
    let mut merged = 0usize;
    for row in rows {
        if store.node_merge_from_gossip(row).is_ok() {
            merged += 1;
        }
    }
    (merged, forgotten)
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
    // Tombstones first, so a row they cover is refused below.
    let (_, forgotten) = fold(store, &msg.tombstones, &[], cfg.cluster.stale_after_secs);
    if forgotten > 0 {
        tracing::info!("gossip: {peer} forgot {forgotten} node(s) fleet-wide — dropped here too");
    }
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
    let tombstones = store.node_tombstones()?;
    tx.commit()?;
    let reply = GossipReply {
        rows,
        you_appear_as: client_addr.to_string(),
        merged_new,
        merged_known,
        tombstones,
    };
    let json = serde_json::to_string(&reply)?;
    let sha = crate::memory::MemoryStore::content_sha(&json);
    Ok(format!(
        "{}\n{FENCE_B64}\nsha256:{sha}\n{}\n```",
        crate::arbiter::GOSSIP_REPLY_MARKER,
        b64_encode(&json)
    ))
}

/// What one outbound exchange did here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplyFold {
    /// Rows from the reply that merged.
    pub merged: usize,
    /// Rows the reply's tombstones dropped here.
    pub forgotten: usize,
    /// The address the peer saw us arrive from.
    pub you_appear_as: String,
}

/// Fold a gossip REPLY (the answer to our exchange) into our store, and
/// record the echo against the peer that reported it. Returns
/// (rows merged, the address we appear as).
pub fn receive_gossip_reply(
    store: &crate::store::Store,
    peer_name: &str,
    raw: &str,
) -> Result<(usize, String)> {
    let f = fold_gossip_reply(store, peer_name, raw)?;
    Ok((f.merged, f.you_appear_as))
}

/// [`receive_gossip_reply`], with the tombstones it applied counted.
pub fn fold_gossip_reply(
    store: &crate::store::Store,
    peer_name: &str,
    raw: &str,
) -> Result<ReplyFold> {
    let (sha, b64) = fenced_payload(raw, FENCE_B64)?;
    let json = String::from_utf8(b64_decode(&b64)?)?;
    check_sha(&json, &sha)?;
    let reply: GossipReply = serde_json::from_str(&json).context("not a gossip reply")?;
    // One commit for the whole fold, as in `receive_gossip`.
    let tx = store.write_tx()?;
    // The reply path holds no config; it is read only when there is a
    // tombstone to judge.
    let stale_after = if reply.tombstones.is_empty() {
        0
    } else {
        crate::config::load()
            .map(|c| c.cluster.stale_after_secs)
            .unwrap_or_else(|_| crate::config::ClusterSection::default().stale_after_secs)
    };
    let (merged, forgotten) = fold(store, &reply.tombstones, &reply.rows, stale_after);
    // The echo: THIS node, as the peer sees us. Evidence about OUR
    // uplink, recorded against the peer that reported it.
    if !reply.you_appear_as.is_empty() {
        if let Ok(Some(me)) = crate::node::NodeIdentity::load(&crate::config::config_dir()) {
            let _ = store.node_observe(&me.id, &reply.you_appear_as, peer_name);
        }
    }
    tx.commit()?;
    Ok(ReplyFold {
        merged,
        forgotten,
        you_appear_as: reply.you_appear_as,
    })
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
    let (rows, tombstones) = {
        let s = crate::store::lock_timed(store, "gossip::exchange");
        let rows: Vec<GossipNodeRow> = s
            .nodes_list()?
            .iter()
            .map(GossipNodeRow::from_row)
            .collect();
        (rows, s.node_tombstones()?)
    };
    let sent_tombstones = tombstones.len();
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?.id;
    let text = encode_message(&GossipMessage {
        from_node: me.clone(),
        rows,
        tombstones,
    })?;
    let client = crate::a2a::A2AClient::shared();
    let ctx = crate::a2a::new_context_id();
    let result = match client.call(peer_url, bearer, &text, &ctx).await {
        Ok(r) => r,
        Err(e) => {
            // A gossip peer that stopped (the phone after its idle exit)
            // or no longer takes our token is no route for `any` either:
            // the failure stands in the probe cache as a pass did (COMP-1,
            // 0.2.2), instead of a pass from the last exchange standing
            // for five minutes.
            crate::routeprobe::record_failure(peer_url, bearer, &e);
            return Err(e);
        }
    };
    let reply_text = crate::a2a::extract_text(&result);
    if !reply_text
        .trim_start()
        .starts_with(crate::arbiter::GOSSIP_REPLY_MARKER)
    {
        anyhow::bail!("{peer_name} did not answer gossip with a gossip reply");
    }
    let f = {
        let s = crate::store::lock_timed(store, "gossip::exchange_reply");
        fold_gossip_reply(&s, peer_name, &reply_text)?
    };
    // The exchange crossed the peer's token and trusted-list gates, the
    // ones a placed child crosses: as good as a passed route probe.
    crate::routeprobe::record(peer_url, bearer, crate::routeprobe::Probe::Ok);
    let _ = me;
    let mut line = format!("{peer_name}: merged {} row(s)", f.merged);
    if f.forgotten > 0 {
        line.push_str(&format!(", dropped {} forgotten", f.forgotten));
    }
    if sent_tombstones > 0 {
        line.push_str(&format!(", sent {sent_tombstones} tombstone(s)"));
    }
    line.push_str(&format!("; we appear as {}", f.you_appear_as));
    Ok(line)
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
            cores: None,
            io_some: None,
            io_full: None,
            children: None,
            queued: None,
            children_from: None,
        }
    }

    fn a_message() -> String {
        encode_message(&GossipMessage {
            from_node: "aaaa".into(),
            rows: vec![row("aaaa", "vps"), row("bbbb", "laptop")],
            tombstones: Vec::new(),
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

    fn wire_row(id: &str, heartbeat_at: &str) -> GossipNodeRow {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "host": "h", "version": "0.2.0",
            "uptime_class": "ephemeral",
            "registered_at": "2026-11-01 00:00:00",
            "heartbeat_at": heartbeat_at,
        }))
        .unwrap()
    }

    fn ago(secs: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// FLEET-6 forget propagation: a node forgotten on one gateway is
    /// dropped by the peer it gossips with, in both directions of one
    /// exchange, and a third peer that still lists it (a 0.1.6 node
    /// that ignores tombstones) cannot merge it back on either side.
    #[test]
    fn a_forget_travels_both_ways_and_a_relayed_copy_stays_out() {
        let (_cfg_dir, _data, _guard) = crate::testenv::scratch_xdg();
        let open = |name: &str| {
            let dir = tempfile::tempdir().unwrap();
            let store = crate::store::Store::open(&dir.path().join(name)).unwrap();
            (dir, store)
        };
        let (_a_dir, a) = open("a.db");
        let (_b_dir, b) = open("b.db");
        for s in [&a, &b] {
            s.node_merge_from_gossip(&wire_row("retired-x", &ago(7200)))
                .unwrap();
            s.node_merge_from_gossip(&wire_row("retired-y", &ago(7200)))
                .unwrap();
        }
        // A forgets X, B forgets Y.
        a.node_forget_everywhere("retired-x").unwrap();
        b.node_forget_everywhere("retired-y").unwrap();
        let rows = |s: &crate::store::Store| -> Vec<GossipNodeRow> {
            s.nodes_list()
                .unwrap()
                .iter()
                .map(GossipNodeRow::from_row)
                .collect()
        };
        let msg = encode_message(&GossipMessage {
            from_node: "node-a".into(),
            rows: rows(&a),
            tombstones: a.node_tombstones().unwrap(),
        })
        .unwrap();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.memsync_peers = vec!["a".to_string()];
        let reply = receive_gossip(&b, &cfg, "a", "10.0.30.2:4000", &msg).unwrap();
        assert!(
            b.node_get("retired-x").unwrap().is_none(),
            "the peer drops the node the sender forgot"
        );
        let folded = fold_gossip_reply(&a, "b", &reply).unwrap();
        assert_eq!(folded.forgotten, 1, "{folded:?}");
        assert!(
            a.node_get("retired-y").unwrap().is_none(),
            "and the sender drops the one the peer forgot, from the reply"
        );
        // A third, older peer still lists both, with their old heartbeats,
        // and sends no tombstones at all.
        let old_peer = encode_message(&GossipMessage {
            from_node: "node-c".into(),
            rows: vec![
                wire_row("retired-x", &ago(7100)),
                wire_row("retired-y", &ago(7100)),
            ],
            tombstones: Vec::new(),
        })
        .unwrap();
        cfg.cluster.memsync_peers.push("c".into());
        for s in [&a, &b] {
            receive_gossip(s, &cfg, "c", "10.0.30.3:4000", &old_peer).unwrap();
            assert!(s.node_get("retired-x").unwrap().is_none());
            assert!(s.node_get("retired-y").unwrap().is_none());
        }
    }

    /// The tombstone list is an ADDED field, so a 0.1.6 peer and a 0.2.0
    /// one still read each other: nothing forgotten sends exactly the old
    /// payload, an old payload reads as no tombstones, and the old structs
    /// (no such field, no `deny_unknown_fields`) read a new payload.
    #[test]
    fn the_wire_stays_readable_both_ways_with_a_0_1_6_peer() {
        #[derive(Deserialize)]
        struct OldMessage {
            from_node: String,
            rows: Vec<GossipNodeRow>,
        }
        #[derive(Deserialize)]
        struct OldReply {
            rows: Vec<GossipNodeRow>,
            you_appear_as: String,
        }
        let quiet = GossipMessage {
            from_node: "n".into(),
            rows: vec![row("aaaa", "vps")],
            tombstones: Vec::new(),
        };
        let json = serde_json::to_string(&quiet).unwrap();
        assert!(!json.contains("tombstones"), "{json}");
        let back = decode_message(&encode_message(&quiet).unwrap()).unwrap();
        assert!(back.tombstones.is_empty());

        let t = NodeTombstone {
            id: "gone".into(),
            forgotten_at: "2026-11-07 10:00:00".into(),
        };
        let loud = GossipMessage {
            tombstones: vec![t.clone()],
            ..quiet
        };
        let old: OldMessage = serde_json::from_str(&serde_json::to_string(&loud).unwrap())
            .expect("a 0.1.6 peer reads a message with tombstones");
        assert_eq!((old.from_node.as_str(), old.rows.len()), ("n", 1));
        let reply = GossipReply {
            rows: vec![row("bbbb", "laptop")],
            you_appear_as: "10.0.0.1:1".into(),
            merged_new: 0,
            merged_known: 1,
            tombstones: vec![t],
        };
        let old: OldReply = serde_json::from_str(&serde_json::to_string(&reply).unwrap())
            .expect("a 0.1.6 peer reads a reply with tombstones");
        assert_eq!(old.rows.len(), 1);
        assert_eq!(old.you_appear_as, "10.0.0.1:1");
    }

    /// COMP-1 (0.2.2): a gossip exchange that never reached the peer (it
    /// stopped: the phone's idle exit) or that the peer turned away (a
    /// token it no longer takes) is a failed route, as a probe that found
    /// the same would be. Before, only a success was recorded, so a pass
    /// from the last exchange kept the node routable for `any` for up to
    /// five minutes after it went away.
    #[tokio::test]
    async fn a_failed_gossip_exchange_is_a_failed_route() {
        use crate::routeprobe::{cached, record, Probe};
        let (_home, _env) = crate::testenv::scratch_home();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&crate::config::veles_home().join("state.db")).unwrap(),
        ));
        let down = crate::routeprobe::tests::dead_url().await;
        record(&down, None, Probe::Ok);
        assert!(exchange_with_peer(&store, "phone", &down, None)
            .await
            .is_err());
        assert!(
            matches!(cached(&down, None), Some((Probe::Unreachable(_), _))),
            "{:?}",
            cached(&down, None)
        );
        let rotated = crate::routeprobe::tests::fake_peer("vps", Some("new")).await;
        record(&rotated, Some("old"), Probe::Ok);
        assert!(exchange_with_peer(&store, "vps", &rotated, Some("old"))
            .await
            .is_err());
        assert!(
            matches!(cached(&rotated, Some("old")), Some((Probe::Refused(why), _)) if why.contains("401")),
            "{:?}",
            cached(&rotated, Some("old"))
        );
    }
}
